//! The output engine: one connection with the session's single playback stream, served by one
//! real-time thread.
//!
//! Every playback `Request` from the server is answered by the [`Mixer`]: each source's bridge
//! through its processor, summed, through the master processor, in blocks of at most
//! [`MAX_BLOCK_FRAMES`]. The answer is always the full requested length: silence while there
//! are no sources, a bridge (re)buffers or the stream plays into one of Sidestage's own capture
//! sinks (feedback, see [`Levels::output_muted`]), never nothing. After setup the thread does no
//! allocation, locking or logging on the data path; its only syscalls are the socket reads and
//! writes themselves. Sources come and go through the mixer's lock-free queues while it plays.

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

use super::conn::{CONTROL, CallError, Conn, Control, set_str};
use super::mixer::{Mixer, MixerControl, mixer};
use super::modargs::{RATE, is_sidestage_sink, names_sidestage_sink};
use super::routing::{ROLE_KEY, ROLE_OUTPUT};
use super::session::Msg;
use crate::{AtomicLevel, MAX_BLOCK_FRAMES, Processor};

/// Interleaved stereo float32.
const FRAME_BYTES: usize = 8;
/// Output buffer target (with `ADJUST_LATENCY`, this is sink latency plus our stream buffer).
/// Both servers derive the sink latency we ask for as `(tlength - 2 * minreq) / 2`: 12.5 ms
/// here. It must not drop below 512 frames (10.7 ms): PipeWire rounds the graph quantum down to
/// a power of two, so 40 ms (480 frames) would run the device the game shares at 256-frame
/// (5.3 ms) cycles, which crackle under load.
const OUTPUT_TARGET_MS: u32 = 45;
const OUTPUT_MIN_REQUEST_MS: u32 = 10;
/// Fill target of each source's capture-to-output bridge: one capture fragment plus one output
/// request plus scheduling jitter (a null sink and the sound card run on independent clocks,
/// on PipeWire too: its null-audio-sink drives itself from CLOCK_MONOTONIC).
pub const BRIDGE_TARGET_MS: f32 = 40.0;

pub fn ms_to_bytes(ms: u32) -> u32 {
    RATE / 1000 * ms * FRAME_BYTES as u32
}

pub fn bytes_to_ms(bytes: u32) -> f32 {
    bytes as f32 / FRAME_BYTES as f32 * 1000.0 / RATE as f32
}

pub fn stereo() -> SampleSpec {
    SampleSpec { format: SampleFormat::Float32Le, channels: 2, sample_rate: RATE }
}

/// Levels and counters the engines publish for the session status.
#[derive(Default)]
pub struct Levels {
    /// Current game level from the ducking meters (0 when off).
    pub sidechain: AtomicLevel,
    /// Underflows and overflows the server reported on the output stream, over every engine.
    pub server_xruns: AtomicU64,
    /// The output stream plays into a Sidestage capture sink (the server put it there, e.g.
    /// because no real output is left): it would feed back into that source's capture, and the
    /// limited howl would reach the headphones once the stream is moved off, so it plays
    /// silence meanwhile. Set by the engine when the server says where the stream went, and by
    /// the session from its stream listing.
    pub output_muted: AtomicBool,
}

pub struct AudioEngine {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Adds and removes the sources the engine plays.
    pub mixer: MixerControl,
    /// Sink-input index of the output stream (to find which sink it plays on).
    pub output_stream: u32,
    /// Latency of the output side: our stream buffer and the sink's latency.
    pub latency_ms: f32,
    /// The requested output sink does not exist (any more); playing to the default instead.
    pub output_fell_back: bool,
}

impl AudioEngine {
    /// Opens the playback stream (so setup errors reach the caller) and starts the engine
    /// thread with no sources. `output_sink` is a sink name, or `None` to follow the default
    /// output. If that sink is gone (unplugged), the default output is used rather than
    /// failing every restart.
    pub fn start(
        output_sink: Option<&str>,
        master: Box<dyn Processor>,
        levels: Arc<Levels>,
        events: Sender<Msg>,
        generation: u64,
    ) -> anyhow::Result<Self> {
        let mut conn = Conn::connect("output")?;
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
        let latency_ms = playback.stream_latency as f32 / 1000.0 + bytes_to_ms(playback.buffer_attr.target_length);
        let sink_name = playback.sink_name.as_ref().map_or_else(|| "?".into(), |n| n.to_string_lossy());
        log::info!(
            "pulse: playing to {sink_name} (tlength {} B, minreq {} B), ~{latency_ms:.0} ms",
            playback.buffer_attr.target_length,
            playback.buffer_attr.minimum_request_length,
        );
        let into_capture = is_sidestage_sink(&sink_name);
        if into_capture {
            log::warn!("pulse: the output stream opened on a Sidestage capture sink; muted until it is moved off");
        }
        levels.output_muted.store(into_capture, Ordering::Relaxed);

        conn.reserve_stream(MAX_BLOCK_FRAMES * 2);
        let (control, mixer) = mixer(master);
        let stop = Arc::new(AtomicBool::new(false));
        let engine = Engine {
            conn,
            stop: stop.clone(),
            mixer,
            levels,
            playback_channel: playback.channel,
            requested: playback.requested_bytes as usize,
            out: vec![0.0; MAX_BLOCK_FRAMES * 2],
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
            mixer: control,
            output_stream: playback.stream_index,
            latency_ms,
            output_fell_back,
        })
    }
}

impl Drop for AudioEngine {
    /// Stops the thread; closing its connection deletes the stream on the server. The sources
    /// still in the mixer are freed as the thread ends.
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
    mixer: Mixer,
    levels: Arc<Levels>,
    playback_channel: u32,
    /// Bytes the server asked for and has not received yet.
    requested: usize,
    out: Vec<f32>,
    events: Sender<Msg>,
    generation: u64,
}

impl Engine {
    fn run(mut self) {
        match raise_priority() {
            Ok(()) => log::info!("pulse: the audio thread runs with real-time priority"),
            Err(e) => log::info!("pulse: the audio thread keeps normal priority ({e})"),
        }
        // A panicking master processor must not end the thread silently: every source would
        // stay parked on its capture sink with nothing playing it. Report it so the session
        // restarts the engine (or, if it keeps failing, gives the apps their own output back).
        // A panicking source processor is contained by the mixer.
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
            // Also while the server asks for nothing (a suspended or corked sink), so removed
            // sources still go back to the session thread.
            self.mixer.apply_commands();
            if !self.conn.wait_readable()? {
                continue;
            }
            let header = self.conn.read_header()?;
            if header.channel == CONTROL {
                self.conn.read_control(header.length)?;
                self.on_control()?;
            } else {
                self.conn.skip(header.length)?;
            }
            self.answer_requests()?;
        }
        Ok(())
    }

    /// Handles the commands the engine acts on. Everything else (suspended, started, buffer
    /// changes) is skipped undecoded, and a move is only peeked at: decoding some of those
    /// allocates, and the session learns about moves from its own events.
    fn on_control(&mut self) -> anyhow::Result<()> {
        const HANDLED: [CommandTag; 4] =
            [CommandTag::Request, CommandTag::Underflow, CommandTag::Overflow, CommandTag::PlaybackStreamKilled];
        if self.conn.tag() == Some(CommandTag::PlaybackStreamMoved as u32) {
            self.on_moved();
            return Ok(());
        }
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
            // Undecodable or for another channel: nothing to do (and no logging on this thread).
            Ok(_) | Err(_) => {}
        }
        Ok(())
    }

    /// The server moved the output stream (`PlaybackStreamMoved`: tag, sequence, channel, sink
    /// index, sink name, ...). Mute it at once while it plays into a capture sink, rather than
    /// after the session's next listing; the session moves it off. Read undecoded, as decoding
    /// would allocate the sink name.
    fn on_moved(&self) {
        let payload = self.conn.payload();
        // The channel is the third tagged u32.
        if let Some(&[b'L', a, b, c, d]) = payload.get(10..15)
            && u32::from_be_bytes([a, b, c, d]) == self.playback_channel
        {
            self.levels.output_muted.store(names_sidestage_sink(&payload[15..]), Ordering::Relaxed);
        }
    }

    /// Answers outstanding playback requests in whole frames.
    fn answer_requests(&mut self) -> anyhow::Result<()> {
        while self.requested >= FRAME_BYTES {
            let frames = (self.requested / FRAME_BYTES).min(MAX_BLOCK_FRAMES);
            let out = &mut self.out[..frames * 2];
            self.mixer.render(out, self.levels.sidechain.load());
            if self.levels.output_muted.load(Ordering::Relaxed) {
                // The sources still run, so nothing is cut or rebuffered when it is unmuted.
                out.fill(0.0);
            }
            self.conn.write_stream(self.playback_channel, out)?;
            self.requested -= frames * FRAME_BYTES;
        }
        Ok(())
    }
}

/// Real-time priority for the calling (audio) thread, so a game that keeps every core busy
/// cannot starve it into underruns. Best effort: it needs `RLIMIT_RTPRIO` (granted to the
/// `audio` group or by `limits.conf` on many distributions); without it the thread keeps normal
/// priority and the buffers absorb scheduling delays. Priority 1, the lowest real-time level,
/// is enough to run ahead of every normal thread and stays below the sound server's own
/// real-time threads (PulseAudio defaults to 5, PipeWire to 20 or more), which must run first.
/// `SCHED_RESET_ON_FORK` keeps anything this thread starts from inheriting it.
pub fn raise_priority() -> std::io::Result<()> {
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

fn output_params(sink: Option<&str>, version: u16) -> anyhow::Result<protocol::PlaybackStreamParams> {
    let mut props = Props::new();
    set_str(&mut props, Prop::MediaName.to_c_str(), "Sidestage spatial audio");
    set_str(&mut props, ROLE_KEY, ROLE_OUTPUT);
    Ok(protocol::PlaybackStreamParams {
        sample_spec: stereo(),
        channel_map: ChannelMap::stereo(),
        // No sink: the server's default, followed when it changes.
        sink_name: sink.map(std::ffi::CString::new).transpose()?,
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
