//! The audio engine: one connection that records the capture sink's monitor and plays the
//! spatialized result, served by one thread.
//!
//! Captured packets go into the drift-compensating bridge; every playback `Request` from the
//! server is answered by pulling from the bridge and running the [`Processor`] in blocks of at
//! most [`MAX_BLOCK_FRAMES`]. The answer is always the full requested length: silence while the
//! bridge (re)buffers, never nothing. After setup the thread does no allocation, locking or
//! logging on the data path; its only syscalls are the socket reads and writes themselves.

use std::ffi::CString;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use anyhow::{Context, bail};
use pulseaudio::protocol::stream::{BufferAttr, StreamFlags};
use pulseaudio::protocol::{
    self, ChannelMap, ChannelVolume, Command, CommandTag, Prop, Props, PulseError, SampleFormat, SampleSpec,
};

use crate::bridge::{self, BridgeConfig, BridgeConsumer, BridgeProducer, BridgeStats};
use crate::{AtomicLevel, MAX_BLOCK_FRAMES, Processor, peak};

use super::conn::{CONTROL, CallError, Conn, Control, set_str};
use super::modargs::RATE;
use super::pcm::FrameDecoder;
use super::routing::{ROLE_CAPTURE, ROLE_KEY, ROLE_OUTPUT};
use super::session::Msg;

/// Interleaved stereo float32.
const FRAME_BYTES: usize = 8;
/// Capture fragment: how often the server hands us captured audio.
const CAPTURE_FRAGMENT_MS: u32 = 10;
/// Output buffer target (with `ADJUST_LATENCY`, this is sink latency plus our stream buffer).
/// Both servers derive the sink latency we ask for as `(tlength - 2 * minreq) / 2`: 12.5 ms
/// here. It must not drop below 512 frames (10.7 ms): PipeWire rounds the graph quantum down to
/// a power of two, so 40 ms (480 frames) would run the device the game shares at 256-frame
/// (5.3 ms) cycles, which crackle under load.
const OUTPUT_TARGET_MS: u32 = 45;
const OUTPUT_MIN_REQUEST_MS: u32 = 10;
/// Fill target of the capture-to-output bridge: one capture fragment plus one output request
/// plus scheduling jitter (the null sink and the sound card run on independent clocks, on
/// PipeWire too: its null-audio-sink drives itself from CLOCK_MONOTONIC).
const BRIDGE_TARGET_MS: f32 = 40.0;

fn ms_to_bytes(ms: u32) -> u32 {
    RATE / 1000 * ms * FRAME_BYTES as u32
}

fn bytes_to_ms(bytes: u32) -> f32 {
    bytes as f32 / FRAME_BYTES as f32 * 1000.0 / RATE as f32
}

fn stereo() -> SampleSpec {
    SampleSpec { format: SampleFormat::Float32Le, channels: 2, sample_rate: RATE }
}

/// Levels and counters the engines publish for the session status.
#[derive(Default)]
pub struct Levels {
    /// Peak of the captured music since the status last read it.
    pub capture: AtomicLevel,
    /// Current game level from the ducking meters (0 when off).
    pub sidechain: AtomicLevel,
    /// Underflows and overflows the server reported on the output stream.
    pub server_xruns: AtomicU64,
}

pub struct AudioEngine {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub stats: Arc<BridgeStats>,
    /// Sink-input index of the output stream (to find which sink it plays on).
    pub output_stream: u32,
    /// Latency outside the bridge: capture fragment and source latency, output buffer and
    /// sink latency.
    pub base_latency_ms: f32,
    /// The requested output sink does not exist (any more); playing to the default instead.
    pub output_fell_back: bool,
}

impl AudioEngine {
    /// Opens both streams (so setup errors reach the caller) and starts the engine thread.
    /// `output_sink` is a sink name, or `None` to follow the default output. If that sink is
    /// gone (unplugged), the default output is used rather than failing every restart.
    pub fn start(
        monitor_source: &str,
        output_sink: Option<&str>,
        processor: Box<dyn Processor>,
        levels: Arc<Levels>,
        events: Sender<Msg>,
        generation: u64,
    ) -> anyhow::Result<Self> {
        let mut conn = Conn::connect("audio")?;
        let (producer, consumer, stats) = bridge::bridge(BridgeConfig::new(RATE, BRIDGE_TARGET_MS));

        let record: protocol::CreateRecordStreamReply = conn
            .request(&Command::CreateRecordStream(capture_params(monitor_source)?))
            .with_context(|| format!("recording {monitor_source}"))?;
        if record.sample_spec != stereo() {
            bail!("the capture stream was opened as {:?} instead of float32 stereo", record.sample_spec);
        }
        let version = conn.version();
        let mut output_fell_back = false;
        let playback: protocol::CreatePlaybackStreamReply =
            match conn.request(&Command::CreatePlaybackStream(output_params(output_sink, version)?)) {
                Err(CallError::Server(PulseError::NoEntity)) if output_sink.is_some() => {
                    log::warn!(
                        "pulse: output {} is not available; using the default output",
                        output_sink.unwrap_or_default()
                    );
                    output_fell_back = true;
                    conn.request(&Command::CreatePlaybackStream(output_params(None, version)?))
                }
                result => result,
            }
            .context("opening the output stream")?;
        if playback.sample_spec != stereo() {
            bail!("the output stream was opened as {:?} instead of float32 stereo", playback.sample_spec);
        }

        let base_latency_ms = (record.stream_latency + playback.stream_latency) as f32 / 1000.0
            + bytes_to_ms(record.buffer_attr.fragment_size)
            + bytes_to_ms(playback.buffer_attr.target_length);
        log::info!(
            "pulse: capturing {monitor_source} (fragment {} B), playing to {} (tlength {} B, minreq {} B), ~{base_latency_ms:.0} ms + bridge",
            record.buffer_attr.fragment_size,
            playback.sink_name.as_ref().map_or_else(|| "?".into(), |n| n.to_string_lossy()),
            playback.buffer_attr.target_length,
            playback.buffer_attr.minimum_request_length,
        );

        conn.reserve_stream(MAX_BLOCK_FRAMES * 2);
        let stop = Arc::new(AtomicBool::new(false));
        let engine = Engine {
            conn,
            stop: stop.clone(),
            producer,
            consumer,
            processor,
            levels,
            decoder: FrameDecoder::new(2, 1024),
            record_channel: record.channel,
            playback_channel: playback.channel,
            requested: playback.requested_bytes as usize,
            input: vec![0.0; MAX_BLOCK_FRAMES * 2],
            output: vec![0.0; MAX_BLOCK_FRAMES * 2],
            events,
            generation,
        };
        let thread = std::thread::Builder::new()
            .name("sidestage-audio".into())
            .spawn(move || engine.run())
            .context("starting the audio thread")?;
        Ok(Self {
            stop,
            thread: Some(thread),
            stats,
            output_stream: playback.stream_index,
            base_latency_ms,
            output_fell_back,
        })
    }
}

impl Drop for AudioEngine {
    /// Stops the thread; closing its connection deletes both streams on the server.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            log::error!("pulse: the audio thread panicked");
        }
    }
}

struct Engine {
    conn: Conn,
    stop: Arc<AtomicBool>,
    producer: BridgeProducer,
    consumer: BridgeConsumer,
    processor: Box<dyn Processor>,
    levels: Arc<Levels>,
    decoder: FrameDecoder,
    record_channel: u32,
    playback_channel: u32,
    /// Bytes the server asked for and has not received yet.
    requested: usize,
    input: Vec<f32>,
    output: Vec<f32>,
    events: Sender<Msg>,
    generation: u64,
}

impl Engine {
    fn run(mut self) {
        match raise_priority() {
            Ok(()) => log::info!("pulse: the audio thread runs with real-time priority"),
            Err(e) => log::info!("pulse: the audio thread keeps normal priority ({e})"),
        }
        // A panicking processor must not end the thread silently: the app would stay parked on
        // the capture sink with nothing playing it. Report it so the session restarts the
        // engine (or, if it keeps failing, gives the app its own output back).
        let reason = match panic::catch_unwind(AssertUnwindSafe(|| self.serve())) {
            Ok(Ok(())) => return,
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => "the audio processor crashed".to_owned(),
        };
        log::warn!("pulse: audio engine stopped: {reason}");
        let _ = self.events.send(Msg::EngineEnded { generation: self.generation, reason });
    }

    fn serve(&mut self) -> anyhow::Result<()> {
        self.answer_requests()?;
        while !self.stop.load(Ordering::Relaxed) {
            if !self.conn.wait_readable()? {
                continue;
            }
            let header = self.conn.read_header()?;
            if header.channel == CONTROL {
                self.conn.read_control(header.length)?;
                self.on_control()?;
            } else if header.channel == self.record_channel {
                let Self { conn, decoder, producer, .. } = self;
                conn.read_data(header.length, |bytes| {
                    decoder.feed(bytes, |frames| producer.push(frames));
                })?;
            } else {
                self.conn.skip(header.length)?;
            }
            self.answer_requests()?;
        }
        Ok(())
    }

    /// Handles the commands the engine acts on. Everything else (stream moved, suspended,
    /// started, buffer changes) is skipped undecoded: decoding some of those allocates, and
    /// the session learns about moves from its own events.
    fn on_control(&mut self) -> anyhow::Result<()> {
        const HANDLED: [CommandTag; 5] = [
            CommandTag::Request,
            CommandTag::Underflow,
            CommandTag::Overflow,
            CommandTag::PlaybackStreamKilled,
            CommandTag::RecordStreamKilled,
        ];
        if !self.conn.tag().is_some_and(|tag| HANDLED.iter().any(|&h| h as u32 == tag)) {
            return Ok(());
        }
        match self.conn.decode() {
            Ok(Control::Command(Command::Request(r))) if r.channel == self.playback_channel => {
                self.requested += r.length as usize;
            }
            Ok(Control::Command(Command::Underflow(u))) if u.channel == self.playback_channel => {
                self.levels.server_xruns.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Control::Command(Command::Overflow(channel))) if channel == self.playback_channel => {
                self.levels.server_xruns.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Control::Command(Command::PlaybackStreamKilled(channel))) if channel == self.playback_channel => {
                bail!("the sound server closed Sidestage's output stream");
            }
            Ok(Control::Command(Command::RecordStreamKilled(channel))) if channel == self.record_channel => {
                bail!("the sound server closed Sidestage's capture stream");
            }
            // Undecodable or for another channel: nothing to do (and no logging on this thread).
            Ok(_) | Err(_) => {}
        }
        Ok(())
    }

    /// Answers outstanding playback requests in whole frames.
    fn answer_requests(&mut self) -> anyhow::Result<()> {
        while self.requested >= FRAME_BYTES {
            let frames = (self.requested / FRAME_BYTES).min(MAX_BLOCK_FRAMES);
            let n = frames * 2;
            let input = &mut self.input[..n];
            let output = &mut self.output[..n];
            self.consumer.pull(input);
            self.levels.capture.raise(peak(input));
            self.processor.process(input, output, self.levels.sidechain.load());
            self.conn.write_stream(self.playback_channel, output)?;
            self.requested -= frames * FRAME_BYTES;
        }
        Ok(())
    }
}

/// Real-time priority for the calling (audio) thread, so a game that keeps every core busy
/// cannot starve it into underruns. Best effort: it needs `RLIMIT_RTPRIO` (granted to the
/// `audio` group or by `limits.conf` on many distributions); without it the thread keeps normal
/// priority and the output buffer absorbs scheduling delays. Priority 1, the lowest real-time
/// level, is enough to run ahead of every normal thread and stays below the sound server's own
/// real-time threads (PulseAudio defaults to 5, PipeWire to 20 or more), which must run first.
/// `SCHED_RESET_ON_FORK` keeps anything this thread starts from inheriting it.
fn raise_priority() -> std::io::Result<()> {
    // SAFETY: `sched_param` is plain data (glibc has one field, musl a few more), so all-zero is
    // a valid value.
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    param.sched_priority = 1;
    // SAFETY: changes only the calling thread's scheduling; `param` outlives the call. Returns
    // an error number rather than setting errno.
    let err = unsafe {
        libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO | libc::SCHED_RESET_ON_FORK, &param)
    };
    match err {
        0 => Ok(()),
        e => Err(std::io::Error::from_raw_os_error(e)),
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

fn output_params(sink: Option<&str>, version: u16) -> anyhow::Result<protocol::PlaybackStreamParams> {
    let mut props = Props::new();
    set_str(&mut props, Prop::MediaName.to_c_str(), "Sidestage spatial music");
    set_str(&mut props, ROLE_KEY, ROLE_OUTPUT);
    Ok(protocol::PlaybackStreamParams {
        sample_spec: stereo(),
        channel_map: ChannelMap::stereo(),
        // No sink: the server's default, followed when it changes.
        sink_name: sink.map(CString::new).transpose()?,
        buffer_attr: BufferAttr {
            max_length: u32::MAX,
            target_length: ms_to_bytes(OUTPUT_TARGET_MS),
            pre_buffering: u32::MAX,
            minimum_request_length: ms_to_bytes(OUTPUT_MIN_REQUEST_MS),
            fragment_size: u32::MAX,
        },
        flags: StreamFlags { adjust_latency: true, ..Default::default() },
        // Before protocol 14 the server always applies the volume we send; send unity then.
        // Later, no volume lets the server restore the user's volume for our stream.
        cvolume: (version < 14).then(|| ChannelVolume::norm(2)),
        props,
        ..Default::default()
    })
}
