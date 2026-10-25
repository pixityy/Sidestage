//! One source's capture: a connection of its own that records the source's capture-sink
//! monitor into the source's drift bridge, served by one thread.
//!
//! Each source has its own connection and thread, so a source whose sink goes away (or whose
//! processor crashes) is restarted alone while every other source keeps playing. The data path
//! only copies into the bridge: no allocation, locking or logging, and its only syscalls are
//! the socket reads.

use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use anyhow::{Context, bail};
use pulseaudio::protocol::stream::{BufferAttr, StreamFlags};
use pulseaudio::protocol::{self, ChannelMap, Command, CommandTag, Prop, Props};

use super::audio::{bytes_to_ms, ms_to_bytes, raise_priority, stereo};
use super::conn::{CONTROL, Conn, Control, set_str};
use super::pcm::FrameDecoder;
use super::routing::{ROLE_CAPTURE, ROLE_KEY};
use super::session::Msg;
use crate::SourceId;
use crate::bridge::BridgeProducer;

/// Capture fragment: how often the server hands us captured audio.
const CAPTURE_FRAGMENT_MS: u32 = 10;

pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Latency before the bridge: the monitor source's latency plus one fragment.
    pub latency_ms: f32,
}

impl Capture {
    /// Opens the record stream (so setup errors reach the caller) and starts the thread. When
    /// the stream ends for any reason other than [`Drop`], the thread sends
    /// [`Msg::CaptureEnded`] with `source` and `generation`; it also does so once `crashed` is
    /// set (the mixer saw this source's processor panic).
    pub fn start(
        monitor_source: &str,
        producer: BridgeProducer,
        crashed: Arc<AtomicBool>,
        events: Sender<Msg>,
        source: SourceId,
        generation: u64,
    ) -> anyhow::Result<Self> {
        let mut conn = Conn::connect("capture")?;
        let record: protocol::CreateRecordStreamReply = conn
            .request(&Command::CreateRecordStream(capture_params(monitor_source)?))
            .with_context(|| format!("recording {monitor_source}"))?;
        if record.sample_spec != stereo() {
            bail!("the capture stream was opened as {:?} instead of float32 stereo", record.sample_spec);
        }
        let latency_ms = record.stream_latency as f32 / 1000.0 + bytes_to_ms(record.buffer_attr.fragment_size);
        log::info!(
            "pulse: capturing {monitor_source} (fragment {} B, ~{latency_ms:.0} ms) for source {source}",
            record.buffer_attr.fragment_size
        );
        let stop = Arc::new(AtomicBool::new(false));
        let reader = Reader {
            conn,
            stop: stop.clone(),
            producer,
            decoder: FrameDecoder::new(2, 1024),
            channel: record.channel,
            crashed,
            events,
            source,
            generation,
        };
        let thread = std::thread::Builder::new()
            .name(format!("sidestage-capture-{source}"))
            .spawn(move || reader.run())
            .context("starting a capture thread")?;
        Ok(Self { stop, thread: Some(thread), latency_ms })
    }
}

impl Drop for Capture {
    /// Stops the thread; closing its connection deletes the record stream on the server.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            log::error!("pulse: a capture thread panicked");
        }
    }
}

struct Reader {
    conn: Conn,
    stop: Arc<AtomicBool>,
    producer: BridgeProducer,
    decoder: FrameDecoder,
    channel: u32,
    crashed: Arc<AtomicBool>,
    events: Sender<Msg>,
    source: SourceId,
    generation: u64,
}

impl Reader {
    fn run(mut self) {
        // The bridge holds 40 ms; a game that keeps every core busy could still starve a
        // normal-priority thread for longer than that.
        if let Err(e) = raise_priority() {
            log::debug!("pulse: capture thread keeps normal priority ({e})");
        }
        let reason = match self.serve() {
            Ok(()) => return,
            Err(e) => format!("{e:#}"),
        };
        log::warn!("pulse: capture of source {} stopped: {reason}", self.source);
        let _ = self.events.send(Msg::CaptureEnded { source: self.source, generation: self.generation, reason });
    }

    fn serve(&mut self) -> anyhow::Result<()> {
        while !self.stop.load(Ordering::Relaxed) {
            if self.crashed.load(Ordering::Relaxed) {
                bail!("the source's audio processor crashed");
            }
            if !self.conn.wait_readable()? {
                continue;
            }
            let header = self.conn.read_header()?;
            if header.channel == CONTROL {
                self.conn.read_control(header.length)?;
                self.on_control()?;
            } else if header.channel == self.channel {
                let Self { conn, decoder, producer, .. } = self;
                conn.read_data(header.length, |bytes| {
                    decoder.feed(bytes, |frames| producer.push(frames));
                })?;
            } else {
                self.conn.skip(header.length)?;
            }
        }
        Ok(())
    }

    /// Only a killed stream matters here. Everything else (suspended, moved, buffer changes)
    /// is skipped undecoded: decoding some of those allocates.
    fn on_control(&mut self) -> anyhow::Result<()> {
        if self.conn.tag() != Some(CommandTag::RecordStreamKilled as u32) {
            return Ok(());
        }
        match self.conn.decode() {
            Ok(Control::Command(Command::RecordStreamKilled(channel))) if channel == self.channel => {
                bail!("the sound server closed the capture stream")
            }
            Ok(_) | Err(_) => Ok(()),
        }
    }
}

fn capture_params(monitor_source: &str) -> anyhow::Result<protocol::RecordStreamParams> {
    let mut props = Props::new();
    set_str(&mut props, Prop::MediaName.to_c_str(), "Sidestage capture");
    set_str(&mut props, ROLE_KEY, ROLE_CAPTURE);
    Ok(protocol::RecordStreamParams {
        sample_spec: stereo(),
        channel_map: ChannelMap::stereo(),
        // Exactly one of index and name (PulseAudio rejects both).
        source_name: Some(CString::new(monitor_source)?),
        // pipewire-pulse defaults the fragment to 2 s, so always set it.
        buffer_attr: BufferAttr {
            max_length: u32::MAX,
            fragment_size: ms_to_bytes(CAPTURE_FRAGMENT_MS),
            ..Default::default()
        },
        flags: StreamFlags {
            // Without it, PulseAudio "rescues" the stream to the default source (the
            // microphone) when the capture sink goes away.
            no_move: true,
            adjust_latency: true,
            ..Default::default()
        },
        props,
        ..Default::default()
    })
}
