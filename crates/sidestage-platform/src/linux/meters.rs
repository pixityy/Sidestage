//! Ducking sidechain: one tiny peak-meter stream per game stream.
//!
//! The real sink's monitor would include Sidestage's own output, and nothing can exclude a
//! stream from a monitor. Instead, like pavucontrol's per-stream VU meters, each other
//! sink-input on the output sink gets a record stream bound to it (`direct_on_input`) with
//! `PEAK_DETECT` at 144 Hz mono; the game level is the loudest of them. PulseAudio kills such a
//! meter whenever its stream starts moving, so meters are recreated on demand; the session
//! sends the wanted set after every change.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use pulseaudio::protocol::stream::{BufferAttr, StreamFlags};
use pulseaudio::protocol::{self, ChannelMap, Command, Prop, Props, SampleFormat, SampleSpec, TagStructReader};

use super::audio::Levels;
use super::conn::{CONTROL, Conn, Control, set_str};
use super::pcm::FrameDecoder;
use super::routing::{self, ROLE_KEY, ROLE_METER};

const PEAK_RATE: u32 = 144;
/// Two peak samples (13.9 ms). pipewire-pulse turns a record stream's fragment into its
/// `node.latency` whatever the flags, and the meter joins the game's graph: one sample
/// (6.9 ms, 333 frames) would round that graph's quantum down to 256 frames.
const METER_FRAGMENT_BYTES: u32 = 2 * 4;
/// A meter that has not reported for this long (its stream paused) no longer counts.
const HOLD: Duration = Duration::from_millis(150);
/// Wait before recreating a meter the server killed (its stream is moving).
const RETRY_KILLED: Duration = Duration::from_millis(250);
/// Wait before retrying a meter the server refused (e.g. the stream just ended).
const RETRY_REFUSED: Duration = Duration::from_secs(2);

/// `(sink-input index, monitor source index of the sink it plays on)`.
pub type Target = (u32, u32);

pub struct MeterEngine {
    targets: Sender<Vec<Target>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MeterEngine {
    pub fn start(levels: Arc<Levels>) -> anyhow::Result<Self> {
        let conn = Conn::connect("meters")?;
        let (targets, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let meters = Meters {
            conn,
            rx,
            stop: stop.clone(),
            levels,
            desired: BTreeMap::new(),
            meters: BTreeMap::new(),
            by_channel: BTreeMap::new(),
            pending: BTreeMap::new(),
            retry_at: BTreeMap::new(),
            dirty: true,
            decoder: FrameDecoder::new(1, 64),
        };
        let thread = std::thread::Builder::new()
            .name("sidestage-meters".into())
            .spawn(move || meters.run())
            .context("starting the ducking meter thread")?;
        Ok(Self { targets, stop, thread: Some(thread) })
    }

    pub fn set_targets(&self, targets: Vec<Target>) {
        let _ = self.targets.send(targets);
    }

    /// False once the meter thread has ended (its connection failed), so the session can start
    /// a new one instead of ducking silently stopping.
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for MeterEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            log::error!("pulse: the meter thread panicked");
        }
    }
}

struct Meter {
    source: u32,
    channel: u32,
    peak: f32,
    updated: Instant,
}

struct Meters {
    conn: Conn,
    rx: Receiver<Vec<Target>>,
    stop: Arc<AtomicBool>,
    levels: Arc<Levels>,
    /// sink-input -> monitor source.
    desired: BTreeMap<u32, u32>,
    /// sink-input -> live meter.
    meters: BTreeMap<u32, Meter>,
    by_channel: BTreeMap<u32, u32>,
    /// CreateRecordStream sequence -> (sink-input, source).
    pending: BTreeMap<u32, Target>,
    retry_at: BTreeMap<u32, Instant>,
    dirty: bool,
    decoder: FrameDecoder,
}

impl Meters {
    fn run(mut self) {
        if let Err(e) = self.serve() {
            log::warn!("pulse: ducking meters stopped: {e:#}");
        }
        self.levels.sidechain.store(0.0);
    }

    fn serve(&mut self) -> anyhow::Result<()> {
        while !self.stop.load(Ordering::Relaxed) {
            if let Some(targets) = self.rx.try_iter().last() {
                self.desired = targets.into_iter().collect();
                self.dirty = true;
            }
            let now = Instant::now();
            if self.dirty || self.retry_at.values().any(|&t| now >= t) {
                self.reconcile(now)?;
            }
            if self.conn.wait_readable()? {
                let header = self.conn.read_header()?;
                if header.channel == CONTROL {
                    self.conn.read_control(header.length)?;
                    self.on_control()?;
                } else if let Some(&input) = self.by_channel.get(&header.channel) {
                    let Self { conn, decoder, .. } = self;
                    decoder.reset();
                    let mut packet_peak = 0.0f32;
                    conn.read_data(header.length, |bytes| {
                        packet_peak = packet_peak.max(decoder.feed(bytes, |_| {}));
                    })?;
                    if let Some(m) = self.meters.get_mut(&input) {
                        m.peak = packet_peak;
                        m.updated = Instant::now();
                    }
                } else {
                    self.conn.skip(header.length)?;
                }
            }
            let level = routing::combined_peak(self.meters.values().map(|m| (m.peak, m.updated)), Instant::now(), HOLD);
            self.levels.sidechain.store(level);
        }
        Ok(())
    }

    fn reconcile(&mut self, now: Instant) -> anyhow::Result<()> {
        self.dirty = false;
        let stale: Vec<u32> =
            self.meters.iter().filter(|&(i, m)| self.desired.get(i) != Some(&m.source)).map(|(&i, _)| i).collect();
        for input in stale {
            if let Some(m) = self.meters.remove(&input) {
                self.by_channel.remove(&m.channel);
                self.conn.send(&Command::DeleteRecordStream(m.channel))?;
            }
        }
        self.retry_at.retain(|i, t| now < *t && self.desired.contains_key(i));
        let in_flight: BTreeSet<u32> = self.pending.values().map(|&(i, _)| i).collect();
        let wanted: Vec<Target> = self
            .desired
            .iter()
            .filter(|&(i, _)| !self.meters.contains_key(i) && !in_flight.contains(i) && !self.retry_at.contains_key(i))
            .map(|(&i, &s)| (i, s))
            .collect();
        for (input, source) in wanted {
            let seq = self.conn.send(&Command::CreateRecordStream(meter_params(source, input)))?;
            self.pending.insert(seq, (input, source));
        }
        Ok(())
    }

    fn on_control(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        match self.conn.decode() {
            Ok(Control::Reply { seq }) => {
                let Some((input, source)) = self.pending.remove(&seq) else {
                    return Ok(());
                };
                // Only the channel (the reply's first field) is needed. The whole reply is not
                // decoded: pipewire-pulse reports an invalid source index while the metered
                // stream is not linked to a sink, which the crate rejects, and one such stream
                // must not stop every other meter.
                let mut body = self.conn.reply_body();
                let channel = match TagStructReader::new(&mut body, self.conn.version()).read_u32() {
                    Ok(channel) => channel,
                    Err(e) => {
                        log::debug!("pulse: unreadable meter reply for sink-input #{input}: {e}");
                        self.retry_at.insert(input, now + RETRY_REFUSED);
                        return Ok(());
                    }
                };
                if self.desired.get(&input) == Some(&source) {
                    self.by_channel.insert(channel, input);
                    self.meters.insert(input, Meter { source, channel, peak: 0.0, updated: now });
                } else {
                    self.conn.send(&Command::DeleteRecordStream(channel))?;
                }
            }
            Ok(Control::Error { seq, code }) => {
                if let Some((input, _)) = self.pending.remove(&seq) {
                    log::debug!("pulse: meter for sink-input #{input} refused: {code}");
                    self.retry_at.insert(input, now + RETRY_REFUSED);
                }
            }
            Ok(Control::Command(Command::RecordStreamKilled(channel))) => {
                if let Some(input) = self.by_channel.remove(&channel) {
                    self.meters.remove(&input);
                    self.retry_at.insert(input, now + RETRY_KILLED);
                }
            }
            Ok(_) => {}
            Err(e) => log::debug!("pulse: meter connection ignored a message: {e}"),
        }
        Ok(())
    }
}

fn meter_params(source: u32, input: u32) -> protocol::RecordStreamParams {
    let mut props = Props::new();
    set_str(&mut props, Prop::MediaName.to_c_str(), "Sidestage game level");
    set_str(&mut props, ROLE_KEY, ROLE_METER);
    protocol::RecordStreamParams {
        sample_spec: SampleSpec { format: SampleFormat::Float32Le, channels: 1, sample_rate: PEAK_RATE },
        channel_map: ChannelMap::mono(),
        source_index: Some(source),
        direct_on_input_index: Some(input),
        buffer_attr: BufferAttr { max_length: u32::MAX, fragment_size: METER_FRAGMENT_BYTES, ..Default::default() },
        // pavucontrol's recipe without ADJUST_LATENCY: with it, this tiny fragment would ask
        // the *game's* sink for a few milliseconds of latency.
        flags: StreamFlags { no_move: true, peak_detect: true, no_inhibit_auto_suspend: true, ..Default::default() },
        props,
        ..Default::default()
    }
}
