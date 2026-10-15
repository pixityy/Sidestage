//! A simulated backend for `sidestage --demo` and screenshot mode: fake apps with animated
//! levels and a fake session that feeds synthetic music through the real renderer, so the UI
//! can be shown and tested on any machine without capturing audio.

use std::f32::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sidestage_dsp::{Settings, SharedParams};
use sidestage_platform::{
    AppKey, AtomicLevel, AudioApp, HideMethod, OutputDevice, Platform, ProcessorFactory, Session, SessionConfig,
    SessionState, SessionStatus, Support, peak,
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
}

const SAMPLE_RATE: u32 = 48_000;
const BLOCK_FRAMES: usize = 480;

#[derive(Clone, Copy)]
enum Pattern {
    Music,
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

const APPS: [FakeApp; 5] = [
    FakeApp {
        key: "demo:spotify",
        name: "Spotify",
        detail: "Neon Coast \u{2014} Night Drive",
        pid: 4120,
        pattern: Pattern::Music,
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

/// Cheap deterministic "noise" in 0..1 for animation.
fn hash01(n: u32) -> f32 {
    let mut x = n.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xFFFF) as f32 / 65535.0
}

/// Linear peak level of a fake app at time `t` (seconds).
fn level(pattern: Pattern, t: f32) -> f32 {
    match pattern {
        Pattern::Music => {
            // 120 bpm pulse with some variation per beat.
            let beat = t * 2.0;
            let phase = beat.fract();
            let accent = 0.55 + 0.25 * hash01(beat as u32);
            accent * (0.55 + 0.45 * (-phase * 5.0).exp())
        }
        Pattern::Video => {
            if (t / 9.0) as u32 % 3 == 2 {
                0.0
            } else {
                0.25 + 0.2 * (t * 1.7).sin().abs() + 0.1 * hash01((t * 8.0) as u32)
            }
        }
        Pattern::Voice => {
            let word = (t * 3.0) as u32;
            if hash01(word / 4) > 0.55 { 0.15 + 0.35 * hash01(word) } else { 0.0 }
        }
        Pattern::Game => {
            // Ambience plus bursts of gunfire every few seconds.
            let ambience = 0.04 + 0.03 * hash01((t * 6.0) as u32);
            let cycle = t % 6.0;
            let burst = if cycle > 3.5 && cycle < 4.6 { 0.55 + 0.4 * hash01((t * 14.0) as u32) } else { 0.0 };
            ambience.max(burst)
        }
        Pattern::Silent => 0.0,
    }
}

pub struct DemoPlatform {
    scenario: Scenario,
    /// When set and the renderer passes audio through untouched (a stand-in renderer that does
    /// not meter), the demo session reports simulated meters and ducking itself, so the UI still
    /// shows them. A real renderer meters itself and is left alone.
    params: Option<Arc<SharedParams>>,
    epoch: Instant,
    /// Number of `start` calls, for tests.
    starts: Arc<AtomicUsize>,
}

impl DemoPlatform {
    pub fn new(scenario: Scenario, params: Option<Arc<SharedParams>>) -> Self {
        Self { scenario, params, epoch: Instant::now(), starts: Arc::default() }
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
                    playing: l > 0.01,
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
            (HideMethod::Auto, "Turn the original down to silent (recommended)".into()),
            (HideMethod::ParkOn(OUTPUTS[2].0.into()), format!("Send the original to {}", OUTPUTS[2].1)),
            (HideMethod::KeepOriginal, "Keep the original audible (for comparison)".into()),
        ]
    }

    fn start(&mut self, config: SessionConfig, processor: ProcessorFactory) -> anyhow::Result<Box<dyn Session>> {
        self.starts.fetch_add(1, Ordering::Relaxed);
        // Starting takes a moment on real backends; let the UI show its progress state.
        std::thread::sleep(Duration::from_millis(250));
        if self.scenario.start_error {
            anyhow::bail!("The output device is being used exclusively by another app (simulated).");
        }
        let app = APPS
            .iter()
            .find(|a| a.key == config.app.0)
            .ok_or_else(|| anyhow::anyhow!("{} is not running", config.app.0))?;
        let output_name = config
            .output
            .as_deref()
            .and_then(|id| OUTPUTS.iter().find(|(o, _)| *o == id))
            .unwrap_or(&OUTPUTS[0])
            .1
            .to_owned();
        let shared = Arc::new(Shared::default());
        let thread = {
            let shared = Arc::clone(&shared);
            let params = self.params.clone();
            let epoch = self.epoch;
            let sidechain = config.sidechain;
            std::thread::Builder::new()
                .name("sidestage-demo-audio".into())
                .spawn(move || audio_thread(&shared, processor, params, epoch, sidechain))?
        };
        let mut warnings = Vec::new();
        if self.scenario.warnings {
            warnings.push(format!(
                "{} is muted in the system volume mixer, so Sidestage hears silence. Unmute it there and use Sidestage's Volume instead.",
                app.name
            ));
            warnings.push(format!("Spatial sound is turned on for {output_name}; the music would be processed twice."));
        }
        Ok(Box::new(DemoSession {
            shared,
            thread: Some(thread),
            app_name: app.name.into(),
            output_name,
            original_hidden: config.hide != HideMethod::KeepOriginal,
            warnings,
            epoch: self.epoch,
            started: Instant::now(),
            fail_after_start: self.scenario.fail_after_start,
            sidechain: config.sidechain,
        }))
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
    capture_peak: AtomicLevel,
    sidechain_peak: AtomicLevel,
}

struct DemoSession {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    app_name: String,
    output_name: String,
    original_hidden: bool,
    warnings: Vec<String>,
    epoch: Instant,
    started: Instant,
    fail_after_start: bool,
    sidechain: bool,
}

impl DemoSession {
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
        let state = if self.fail_after_start && self.started.elapsed() > Duration::from_millis(150) {
            SessionState::Error("The output device stopped responding (simulated).".into())
        } else {
            SessionState::Running
        };
        SessionStatus {
            state,
            app_name: self.app_name.clone(),
            output_name: self.output_name.clone(),
            sample_rate: SAMPLE_RATE,
            original_hidden: self.original_hidden,
            capture_peak: self.shared.capture_peak.take(),
            sidechain_peak: self.shared.sidechain_peak.take(),
            sidechain_active: self.sidechain,
            latency_ms: 23.0 + (t * 0.7).sin() * 0.4,
            dropouts: 0,
            drift_ppm: (t / 7.0).sin() * 14.0,
            warnings: self.warnings.clone(),
        }
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

/// Synthesizes 10 ms blocks of "music" in real time and runs them through the renderer.
fn audio_thread(
    shared: &Shared,
    factory: ProcessorFactory,
    params: Option<Arc<SharedParams>>,
    epoch: Instant,
    sidechain: bool,
) {
    let mut processor = factory(SAMPLE_RATE);
    let mut input = vec![0.0f32; BLOCK_FRAMES * 2];
    let mut output = vec![0.0f32; BLOCK_FRAMES * 2];
    let mut settings = params.as_ref().map(|p| p.get()).unwrap_or_default();
    let mut duck_db = 0.0f32;
    // Set once the renderer changes the audio: then it is the real one, which reports its own
    // meters (and its real ducking amount must not be overwritten).
    let mut renderer_meters = false;
    let mut sample: u64 = 0;
    let block = Duration::from_secs_f64(BLOCK_FRAMES as f64 / f64::from(SAMPLE_RATE));
    let mut next = Instant::now();

    while !shared.stop.load(Ordering::Relaxed) {
        let t = epoch.elapsed().as_secs_f32();
        synthesize(&mut input, sample, level(Pattern::Music, t));
        sample += BLOCK_FRAMES as u64;
        let sc = if sidechain { level(Pattern::Game, t) } else { 0.0 };
        processor.process(&input, &mut output, sc);
        renderer_meters |= output != input;

        let in_peak = peak(&input);
        shared.capture_peak.raise(in_peak);
        shared.sidechain_peak.raise(sc);
        if let Some(p) = params.as_ref().filter(|_| !renderer_meters) {
            if let Some(s) = p.try_get() {
                settings = s;
            }
            duck_db = simulated_duck(&settings, sc, duck_db, block.as_secs_f32());
            p.meters.report(in_peak, peak(&output), sc, duck_db);
        }

        next += block;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}

/// A few detuned partials over a slow chord change, gated by `amp`.
fn synthesize(buf: &mut [f32], start: u64, amp: f32) {
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
            let phase = (n as f64 * f64::from(*f) / sr).fract() as f32;
            let s = (TAU * phase).sin();
            let pan = k as f32 / 2.0;
            l += s * (1.0 - pan * 0.6);
            r += s * (0.4 + pan * 0.6);
        }
        frame[0] = l * amp * 0.25;
        frame[1] = r * amp * 0.25;
    }
}

/// Approximation of the renderer's ducker for the demo meters (see `research_prior-art.json` C).
fn simulated_duck(s: &Settings, sidechain_peak: f32, current_db: f32, dt: f32) -> f32 {
    let d = s.ducking;
    let target = if d.enabled && sidechain_peak > 0.0 {
        let level_db = 20.0 * sidechain_peak.log10();
        ((level_db - d.threshold_db) * 0.5).clamp(0.0, d.depth_db)
    } else {
        0.0
    };
    let ms = if target > current_db { d.attack_ms } else { d.release_ms };
    let coeff = 1.0 - (-dt * 1000.0 / ms.max(1.0)).exp();
    current_db + (target - current_db) * coeff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_are_in_range() {
        for i in 0..2000 {
            let t = i as f32 * 0.05;
            for p in [Pattern::Music, Pattern::Video, Pattern::Voice, Pattern::Game, Pattern::Silent] {
                let l = level(p, t);
                assert!((0.0..=1.0).contains(&l), "{l}");
            }
        }
    }
}
