//! A simulated backend for `sidestage --demo` and screenshot mode: fake apps with animated
//! levels and a fake session that synthesizes each source's audio (music, a podcast, voice chat
//! ...) and feeds it through the real renderers and master bus, so the UI (meters, ducking under
//! the game and under voice chat) can be shown and tested on any machine without capturing audio.

use std::f32::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sidestage_platform::{
    AppKey, AtomicLevel, AudioApp, HideMethod, OutputDevice, Platform, Processor, Session, SessionConfig, SessionState,
    SessionStatus, SourceConfig, SourceId, SourceStatus, Support, peak,
};

/// Variations for screenshots and manual testing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scenario {
    /// The session reports warnings and `recover` reports something it restored.
    pub warnings: bool,
    /// `start` fails.
    pub start_error: bool,
    /// `start` succeeds, but the session reports an error a moment later.
    pub fail_after_start: bool,
    /// The Discord source fails (its audio cannot be captured).
    pub source_error: bool,
}

const SAMPLE_RATE: u32 = 48_000;
const BLOCK_FRAMES: usize = 480;
/// What the simulated output adds, and what each capture adds on top.
const OUTPUT_LATENCY_MS: f32 = 14.0;
const CAPTURE_LATENCY_MS: f32 = 48.0;
/// A new source reports `Starting` this long, like a real capture that takes a moment.
const SOURCE_START: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pattern {
    Music,
    Podcast,
    Video,
    Voice,
    Game,
    Silent,
}

struct FakeApp {
    key: &'static str,
    name: &'static str,
    detail: &'static str,
    pid: u32,
    pattern: Pattern,
}

const APPS: [FakeApp; 6] = [
    FakeApp {
        key: "demo:spotify",
        name: "Spotify",
        detail: "Neon Coast \u{2014} Night Drive",
        pid: 4120,
        pattern: Pattern::Music,
    },
    FakeApp {
        key: "demo:pocketcasts",
        name: "Pocket Casts",
        detail: "The Patch Notes \u{00b7} Ep. 112",
        pid: 6020,
        pattern: Pattern::Podcast,
    },
    FakeApp {
        key: "demo:firefox",
        name: "Firefox",
        detail: "YouTube \u{00b7} lofi radio",
        pid: 8812,
        pattern: Pattern::Video,
    },
    FakeApp { key: "demo:discord", name: "Discord", detail: "Voice channel", pid: 2304, pattern: Pattern::Voice },
    FakeApp { key: "demo:cs2", name: "Counter-Strike 2", detail: "Game", pid: 15520, pattern: Pattern::Game },
    FakeApp { key: "demo:system", name: "System sounds", detail: "Notifications", pid: 4, pattern: Pattern::Silent },
];

const OUTPUTS: [(&str, &str); 3] = [
    ("demo:out:headphones", "Headphones (USB Audio)"),
    ("demo:out:speakers", "Speakers (High Definition Audio)"),
    ("demo:out:hdmi", "Monitor (HDMI Audio)"),
];

fn fake_app(key: &AppKey) -> Option<(usize, &'static FakeApp)> {
    APPS.iter().enumerate().find(|(_, a)| key.same_app(&AppKey(a.key.into())))
}

/// Cheap deterministic "noise" in 0..1 for animation.
fn hash01(n: u32) -> f32 {
    let mut x = n.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xFFFF) as f32 / 65535.0
}

/// Linear peak level of a fake app at time `t` (seconds since the backend was created).
fn level(pattern: Pattern, t: f32) -> f32 {
    match pattern {
        Pattern::Music => {
            // 120 bpm pulse with some variation per beat.
            let beat = t * 2.0;
            let phase = beat.fract();
            let accent = 0.55 + 0.25 * hash01(beat as u32);
            accent * (0.55 + 0.45 * (-phase * 5.0).exp())
        }
        Pattern::Podcast => {
            // Steady talking with a breath every few seconds.
            let word = (t * 3.2) as u32;
            if (t / 6.5).fract() > 0.9 || hash01(word.wrapping_mul(7)) < 0.08 {
                0.0
            } else {
                0.22 + 0.14 * hash01(word)
            }
        }
        Pattern::Video => {
            if (t / 9.0) as u32 % 3 == 2 {
                0.0
            } else {
                0.25 + 0.2 * (t * 1.7).sin().abs() + 0.1 * hash01((t * 8.0) as u32)
            }
        }
        Pattern::Voice => {
            // A teammate talking in bursts. The first burst starts a second after launch, so
            // screenshots show the music turned down under it.
            let cycle = t % 8.0;
            let talking = (1.0..3.8).contains(&cycle) || (5.2..6.3).contains(&cycle);
            let word = (t * 3.5) as u32;
            if talking && hash01(word.wrapping_mul(13)) > 0.12 { 0.2 + 0.3 * hash01(word) } else { 0.0 }
        }
        Pattern::Game => {
            // Ambience plus bursts of gunfire every few seconds. The ambience (-42 to -37 dBFS)
            // stays under the Standard threshold, so the sources only turn down for the bursts
            // and for voice chat (Competitive, at -42 dB, also reacts to the ambience).
            let ambience = 0.008 + 0.006 * hash01((t * 6.0) as u32);
            let cycle = t % 6.0;
            let burst = if cycle > 3.5 && cycle < 4.6 { 0.55 + 0.4 * hash01((t * 14.0) as u32) } else { 0.0 };
            ambience.max(burst)
        }
        Pattern::Silent => 0.0,
    }
}

pub struct DemoPlatform {
    scenario: Scenario,
    epoch: Instant,
    /// Number of `start` calls, for tests.
    starts: Arc<AtomicUsize>,
}

impl DemoPlatform {
    pub fn new(scenario: Scenario) -> Self {
        Self { scenario, epoch: Instant::now(), starts: Arc::default() }
    }

    /// Count `start` calls into `counter`.
    #[cfg(test)]
    pub fn with_start_counter(mut self, counter: Arc<AtomicUsize>) -> Self {
        self.starts = counter;
        self
    }
}

impl Platform for DemoPlatform {
    fn name(&self) -> &'static str {
        "Demo \u{00b7} simulated apps and audio"
    }

    fn support(&self) -> Support {
        Support::Supported
    }

    fn list_apps(&mut self) -> anyhow::Result<Vec<AudioApp>> {
        let t = self.epoch.elapsed().as_secs_f32();
        Ok(APPS
            .iter()
            .map(|a| {
                let l = level(a.pattern, t);
                AudioApp {
                    key: AppKey(a.key.into()),
                    name: a.name.into(),
                    detail: a.detail.into(),
                    pids: vec![a.pid],
                    playing: l > 0.01 || a.pattern == Pattern::Voice,
                    peak: Some(l),
                }
            })
            .collect())
    }

    fn list_outputs(&mut self) -> anyhow::Result<Vec<OutputDevice>> {
        Ok(OUTPUTS
            .iter()
            .enumerate()
            .map(|(i, (id, name))| OutputDevice { id: (*id).into(), name: (*name).into(), is_default: i == 0 })
            .collect())
    }

    fn hide_methods(&mut self) -> Vec<(HideMethod, String)> {
        vec![
            (HideMethod::Auto, "Turn the originals down to silent (recommended)".into()),
            (HideMethod::ParkOn(OUTPUTS[2].0.into()), format!("Send the originals to {}", OUTPUTS[2].1)),
            (HideMethod::KeepOriginal, "Keep the originals audible (for comparison)".into()),
        ]
    }

    fn start(&mut self, config: SessionConfig) -> anyhow::Result<Box<dyn Session>> {
        self.starts.fetch_add(1, Ordering::Relaxed);
        // Starting takes a moment on real backends; let the UI show its progress state.
        std::thread::sleep(Duration::from_millis(250));
        if self.scenario.start_error {
            anyhow::bail!("The output device is being used exclusively by another app (simulated).");
        }
        if let Some(e) = SessionConfig::duplicate_error(&config.sources) {
            anyhow::bail!(e);
        }
        let output_name = config
            .output
            .as_deref()
            .and_then(|id| OUTPUTS.iter().find(|(o, _)| *o == id))
            .unwrap_or(&OUTPUTS[0])
            .1
            .to_owned();
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel();
        let mut session = DemoSession {
            shared: Arc::clone(&shared),
            tx,
            thread: None,
            sources: Vec::new(),
            ignore: config.sidechain_ignore.clone(),
            output_name,
            scenario: self.scenario,
            epoch: self.epoch,
            started: Instant::now(),
            sidechain: config.sidechain,
        };
        let initial: Vec<RtSource> = config.sources.iter().map(|c| session.add(c)).collect();
        session.update_mask();
        let master = (config.master)(SAMPLE_RATE);
        let epoch = self.epoch;
        let sidechain = config.sidechain;
        session.thread = Some(
            std::thread::Builder::new()
                .name("sidestage-demo-audio".into())
                .spawn(move || audio_thread(&shared, &rx, master, initial, epoch, sidechain))?,
        );
        Ok(Box::new(session))
    }

    fn recover(&mut self) -> anyhow::Result<Vec<String>> {
        Ok(if self.scenario.warnings {
            vec!["Spotify's volume was set back to 100% (Sidestage had closed while hiding it).".into()]
        } else {
            Vec::new()
        })
    }
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    sidechain_peak: AtomicLevel,
    /// Bit `i` set: `APPS[i]` counts as the game (it is neither a source nor ignored).
    game_apps: AtomicU32,
}

/// A source as the session tracks it.
struct DemoSource {
    id: SourceId,
    app: AppKey,
    hide: HideMethod,
    name: String,
    running: bool,
    added: Instant,
    meter: Arc<AtomicLevel>,
}

/// A source on the audio thread.
struct RtSource {
    id: SourceId,
    pattern: Option<Pattern>,
    processor: Box<dyn Processor>,
    meter: Arc<AtomicLevel>,
    synth: Synth,
}

enum Change {
    Add(RtSource),
    Remove(SourceId),
    /// The sources in the session's order.
    Order(Vec<SourceId>),
}

struct DemoSession {
    shared: Arc<Shared>,
    tx: Sender<Change>,
    thread: Option<JoinHandle<()>>,
    sources: Vec<DemoSource>,
    ignore: Vec<AppKey>,
    output_name: String,
    scenario: Scenario,
    epoch: Instant,
    started: Instant,
    sidechain: bool,
}

impl DemoSession {
    /// Track a new source and build its audio-thread half (off the audio thread, as real
    /// backends do: the factory allocates).
    fn add(&mut self, config: &SourceConfig) -> RtSource {
        let found = fake_app(&config.app);
        let name = found.map_or_else(
            || config.app.0.rsplit([':', '\\', '/']).next().unwrap_or_default().to_owned(),
            |(_, a)| a.name.to_owned(),
        );
        let meter = Arc::new(AtomicLevel::new());
        // A source that cannot be captured hears nothing, like on a real backend.
        let failed = self.fails(&config.app);
        self.sources.push(DemoSource {
            id: config.id,
            app: config.app.clone(),
            hide: config.hide.clone(),
            name,
            running: found.is_some(),
            added: Instant::now(),
            meter: Arc::clone(&meter),
        });
        RtSource {
            id: config.id,
            pattern: found.map(|(_, a)| a.pattern).filter(|_| !failed),
            processor: (config.processor)(SAMPLE_RATE),
            meter,
            synth: Synth::new(config.id),
        }
    }

    /// The source of this app fails in the `source_error` scenario.
    fn fails(&self, app: &AppKey) -> bool {
        self.scenario.source_error && app.same_app(&AppKey("demo:discord".into()))
    }

    /// The sidechain hears every fake app that is neither a source nor ignored.
    fn update_mask(&self) {
        let mut mask = 0u32;
        for (i, a) in APPS.iter().enumerate() {
            let key = AppKey(a.key.into());
            let source = self.sources.iter().any(|s| s.app.same_app(&key));
            let ignored = self.ignore.iter().any(|k| k.same_app(&key));
            if !source && !ignored {
                mask |= 1 << i;
            }
        }
        self.shared.game_apps.store(mask, Ordering::Relaxed);
    }

    fn halt(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            log::error!("demo audio thread panicked");
        }
    }
}

impl Session for DemoSession {
    fn status(&self) -> SessionStatus {
        let t = self.epoch.elapsed().as_secs_f32();
        let state = if self.scenario.fail_after_start && self.started.elapsed() > Duration::from_millis(150) {
            SessionState::Error("The output device stopped responding (simulated).".into())
        } else {
            SessionState::Running
        };
        let mut warnings = Vec::new();
        if self.scenario.warnings {
            warnings.push(format!(
                "Spatial sound is turned on for {}; your sources would be processed twice.",
                self.output_name
            ));
        }
        // The warnings scenario's per-source warning goes to the first source that plays.
        let warned = self.sources.iter().position(|s| s.running && !self.fails(&s.app));
        let sources = self
            .sources
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let failed = self.fails(&s.app);
                let state = if failed {
                    SessionState::Error(format!("{}'s audio is protected and can't be captured (simulated).", s.name))
                } else if !s.running {
                    SessionState::WaitingForApp
                } else if s.added.elapsed() < SOURCE_START {
                    SessionState::Starting
                } else {
                    SessionState::Running
                };
                let live = state == SessionState::Running;
                let mut warnings = Vec::new();
                if self.scenario.warnings && Some(i) == warned {
                    warnings.push(format!(
                        "{} is muted in the system volume mixer, so Sidestage hears silence. Unmute it there and use Sidestage's Volume instead.",
                        s.name
                    ));
                }
                SourceStatus {
                    id: s.id,
                    app_name: s.name.clone(),
                    original_hidden: live && s.hide != HideMethod::KeepOriginal,
                    capture_peak: s.meter.take(),
                    latency_ms: OUTPUT_LATENCY_MS + CAPTURE_LATENCY_MS + (t * 0.7 + i as f32).sin() * 0.4,
                    dropouts: 0,
                    drift_ppm: if live { ((t + 3.0 * i as f32) / 7.0).sin() * 14.0 } else { 0.0 },
                    warnings,
                    state,
                }
            })
            .collect();
        SessionStatus {
            state,
            output_name: self.output_name.clone(),
            sample_rate: SAMPLE_RATE,
            sidechain_peak: self.shared.sidechain_peak.take(),
            sidechain_active: self.sidechain,
            latency_ms: OUTPUT_LATENCY_MS,
            dropouts: 0,
            warnings,
            sources,
        }
    }

    fn set_sources(&mut self, sources: Vec<SourceConfig>) -> anyhow::Result<()> {
        if let Some(e) = SessionConfig::duplicate_error(&sources) {
            anyhow::bail!(e);
        }
        // Kept: same ID, app and hide method. Everything else is removed and (re)added.
        let kept = |s: &DemoSource| sources.iter().any(|c| c.id == s.id && c.app == s.app && c.hide == s.hide);
        let mut changes = Vec::new();
        self.sources.retain(|s| {
            let keep = kept(s);
            if !keep {
                changes.push(Change::Remove(s.id));
            }
            keep
        });
        for c in &sources {
            if !self.sources.iter().any(|s| s.id == c.id) {
                let rt = self.add(c);
                changes.push(Change::Add(rt));
            }
        }
        self.sources.sort_by_key(|s| sources.iter().position(|c| c.id == s.id));
        changes.push(Change::Order(sources.iter().map(|c| c.id).collect()));
        self.update_mask();
        for change in changes {
            // A finished audio thread just means the session is being stopped.
            let _ = self.tx.send(change);
        }
        Ok(())
    }

    fn set_sidechain_ignore(&mut self, apps: Vec<AppKey>) -> anyhow::Result<()> {
        self.ignore = apps;
        self.update_mask();
        Ok(())
    }

    fn stop(mut self: Box<Self>) -> anyhow::Result<()> {
        self.halt();
        Ok(())
    }
}

impl Drop for DemoSession {
    fn drop(&mut self) {
        self.halt();
    }
}

/// Mixes the sources in real time, 10 ms at a time, like a backend's render thread: each
/// source's synthesized capture through its renderer, the sum through the master bus.
fn audio_thread(
    shared: &Shared,
    changes: &Receiver<Change>,
    mut master: Box<dyn Processor>,
    mut sources: Vec<RtSource>,
    epoch: Instant,
    sidechain: bool,
) {
    let mut input = vec![0.0f32; BLOCK_FRAMES * 2];
    let mut output = vec![0.0f32; BLOCK_FRAMES * 2];
    let mut mix = vec![0.0f32; BLOCK_FRAMES * 2];
    let mut played = vec![0.0f32; BLOCK_FRAMES * 2];
    let block = Duration::from_secs_f64(BLOCK_FRAMES as f64 / f64::from(SAMPLE_RATE));
    let mut next = Instant::now();

    while !shared.stop.load(Ordering::Relaxed) {
        // A demo convenience a real render thread would not afford: removed renderers are
        // dropped here.
        while let Ok(change) = changes.try_recv() {
            match change {
                Change::Add(source) => sources.push(source),
                Change::Remove(id) => sources.retain(|s| s.id != id),
                Change::Order(order) => sources.sort_by_key(|s| order.iter().position(|id| *id == s.id)),
            }
        }
        let t = epoch.elapsed().as_secs_f32();
        let mask = shared.game_apps.load(Ordering::Relaxed);
        let game = if sidechain {
            APPS.iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, a)| level(a.pattern, t))
                .fold(0.0f32, f32::max)
        } else {
            0.0
        };
        shared.sidechain_peak.raise(game);

        mix.fill(0.0);
        for s in &mut sources {
            match s.pattern {
                Some(p) => s.synth.render(&mut input, p, t),
                None => input.fill(0.0),
            }
            s.meter.raise(peak(&input));
            s.processor.process(&input, &mut output, game);
            mix.iter_mut().zip(&output).for_each(|(m, o)| *m += o);
        }
        master.process(&mix, &mut played, game);

        next += block;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}

/// Synthesized capture for one source.
struct Synth {
    sample: u64,
    /// Glottal phase of the talking voice, in cycles.
    phase: f64,
    seed: u32,
    noise: u32,
}

impl Synth {
    fn new(seed: u32) -> Self {
        Self { sample: 0, phase: 0.0, seed, noise: seed.wrapping_mul(0x2545_F491) | 1 }
    }

    fn render(&mut self, buf: &mut [f32], pattern: Pattern, t: f32) {
        let amp = level(pattern, t);
        match pattern {
            Pattern::Music => chords(buf, self.sample, amp, 1.0),
            Pattern::Video => chords(buf, self.sample, amp, 1.5),
            Pattern::Voice => self.talk(buf, amp, 165.0, t),
            Pattern::Podcast => self.talk(buf, amp, 118.0, t),
            Pattern::Game => {
                for frame in buf.as_chunks_mut::<2>().0 {
                    // xorshift noise for gunfire and ambience
                    self.noise ^= self.noise << 13;
                    self.noise ^= self.noise >> 17;
                    self.noise ^= self.noise << 5;
                    let n = self.noise as f32 / u32::MAX as f32 * 2.0 - 1.0;
                    *frame = [n * amp, n * amp * 0.8];
                }
            }
            Pattern::Silent => buf.fill(0.0),
        }
        self.sample += (buf.len() / 2) as u64;
    }

    /// A buzzy voice: harmonics of a gliding pitch shaped by three vowel formants that change
    /// with every syllable, gated per syllable.
    fn talk(&mut self, buf: &mut [f32], amp: f32, pitch: f32, t: f32) {
        const HARMONICS: usize = 14;
        let syllable = (t * 4.0) as u32;
        let vowel = hash01(syllable.wrapping_add(self.seed.wrapping_mul(31)));
        let f0 = pitch * (1.0 + 0.1 * (t * 1.3 + self.seed as f32).sin() + 0.06 * vowel);
        let formants = [(500.0 + 400.0 * vowel, 160.0), (1100.0 + 900.0 * (1.0 - vowel), 260.0), (2600.0, 420.0)];
        let mut weights = [0.0f32; HARMONICS];
        for (k, w) in weights.iter_mut().enumerate() {
            let f = f0 * (k + 1) as f32;
            *w = formants.iter().map(|(c, bw)| (-((f - c) / bw).powi(2)).exp()).sum::<f32>() + 0.05;
        }
        let norm = weights.iter().sum::<f32>().max(1e-3);
        let sr = f64::from(SAMPLE_RATE);
        for (i, frame) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let ts = t + i as f32 / SAMPLE_RATE as f32;
            let env = (std::f32::consts::PI * (ts * 4.0).fract()).sin().sqrt();
            self.phase = (self.phase + f64::from(f0) / sr).fract();
            let p = self.phase as f32;
            let s: f32 = weights.iter().enumerate().map(|(k, w)| w * (TAU * p * (k + 1) as f32).sin()).sum();
            let v = s / norm * amp * env;
            *frame = [v, v];
        }
    }
}

/// A few detuned partials over a slow chord change, gated by `amp`.
fn chords(buf: &mut [f32], start: u64, amp: f32, transpose: f32) {
    const CHORDS: [[f32; 3]; 4] =
        [[220.0, 277.18, 329.63], [196.0, 246.94, 293.66], [174.61, 220.0, 261.63], [196.0, 246.94, 311.13]];
    let sr = f64::from(SAMPLE_RATE);
    for (i, frame) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let n = start + i as u64;
        let chord = CHORDS[((n / (u64::from(SAMPLE_RATE) * 2)) % 4) as usize];
        let mut l = 0.0;
        let mut r = 0.0;
        for (k, f) in chord.iter().enumerate() {
            // Phase in f64 so long demo runs stay clean.
            let phase = (n as f64 * f64::from(*f * transpose) / sr).fract() as f32;
            let s = (TAU * phase).sin();
            let pan = k as f32 / 2.0;
            l += s * (1.0 - pan * 0.6);
            r += s * (0.4 + pan * 0.6);
        }
        frame[0] = l * amp * 0.25;
        frame[1] = r * amp * 0.25;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processor::{VoiceBus, master_factory, source_factory};
    use sidestage_dsp::{Preset, SharedParams};

    const PATTERNS: [Pattern; 6] =
        [Pattern::Music, Pattern::Podcast, Pattern::Video, Pattern::Voice, Pattern::Game, Pattern::Silent];

    #[test]
    fn levels_are_in_range() {
        for i in 0..2000 {
            let t = i as f32 * 0.05;
            for p in PATTERNS {
                let l = level(p, t);
                assert!((0.0..=1.0).contains(&l), "{l}");
            }
        }
    }

    #[test]
    fn synthesized_audio_stays_within_its_level() {
        let mut buf = vec![0.0f32; BLOCK_FRAMES * 2];
        for p in PATTERNS {
            let mut synth = Synth::new(3);
            for i in 0..300 {
                let t = i as f32 * 0.01;
                synth.render(&mut buf, p, t);
                assert!(buf.iter().all(|s| s.is_finite() && s.abs() <= level(p, t) + 1e-4), "{p:?} at {t}");
            }
        }
    }

    fn config(id: SourceId, key: &str, preset: Preset, bus: &Arc<VoiceBus>) -> (SourceConfig, Arc<SharedParams>) {
        let params = SharedParams::new(preset.settings());
        let processor = source_factory(Arc::clone(&params), Arc::clone(bus), Some(id as usize));
        (SourceConfig { id, app: AppKey(key.into()), hide: HideMethod::Auto, processor }, params)
    }

    #[test]
    fn sessions_play_several_sources_and_change_them_live() {
        let bus = VoiceBus::new();
        let mut platform = DemoPlatform::new(Scenario::default());
        // Discord talks from one second after launch.
        let (music, music_params) = config(1, "demo:spotify", Preset::BehindYou, &bus);
        let (voice, _) = config(2, "demo:discord", Preset::VoiceRight, &bus);
        let (waiting, _) = config(3, "demo:apple-music", Preset::NextRoom, &bus);
        let mut session = platform
            .start(SessionConfig {
                output: None,
                sidechain: true,
                sidechain_ignore: Vec::new(),
                sources: vec![music.clone(), voice.clone(), waiting.clone()],
                master: master_factory(),
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(1800));
        let st = session.status();
        let states: Vec<_> = st.sources.iter().map(|s| (s.id, s.state.clone())).collect();
        assert_eq!(
            states,
            vec![(1, SessionState::Running), (2, SessionState::Running), (3, SessionState::WaitingForApp)]
        );
        assert!(st.sources[0].capture_peak > 0.1 && st.sources[0].original_hidden);
        assert!(st.sidechain_peak > 0.0, "the game counts");
        let duck = music_params.meters.take().duck_db;
        assert!(duck > 1.0, "the music is turned down for the game and Discord: {duck}");

        // Ignoring the game silences the sidechain; removing a source keeps the others.
        session.set_sidechain_ignore(vec![AppKey("demo:cs2".into()), AppKey("demo:firefox".into())]).unwrap();
        session.set_sources(vec![music.clone(), waiting]).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        session.status();
        std::thread::sleep(Duration::from_millis(100));
        let st = session.status();
        assert_eq!(st.sources.iter().map(|s| s.id).collect::<Vec<_>>(), vec![1, 3]);
        assert!(st.sources[0].state == SessionState::Running, "kept sources keep running");
        assert!(st.sidechain_peak > 0.0, "Pocket Casts is neither a source nor ignored");
        session
            .set_sidechain_ignore(vec![
                AppKey("demo:cs2".into()),
                AppKey("demo:pocketcasts".into()),
                AppKey("demo:firefox".into()),
                AppKey("demo:discord".into()),
            ])
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        session.status();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(session.status().sidechain_peak, 0.0, "nothing left that counts as the game");
        assert!(session.set_sources(vec![music.clone(), music]).is_err(), "a source twice is refused");
        session.stop().unwrap();
    }
}
