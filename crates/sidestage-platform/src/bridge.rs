//! Capture-to-output bridge with clock-drift compensation.
//!
//! On Windows and Linux the captured music and the headphone output run on different clocks
//! (two WASAPI streams; a null sink and a sound card). Even at the same nominal rate they drift
//! by tens to hundreds of ppm, so a plain FIFO slowly underruns or overflows. The bridge is a
//! lock-free SPSC ring of interleaved stereo frames. The output side reads it through a cubic
//! (Catmull-Rom) interpolator whose speed is nudged by a proportional controller that holds the
//! averaged fill level at a target, the same idea as PulseAudio's module-loopback.
//!
//! Both ends are real-time safe: no allocation, locks or syscalls after construction.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Bridge tuning. All durations are in milliseconds at `sample_rate`.
#[derive(Clone, Copy, Debug)]
pub struct BridgeConfig {
    pub sample_rate: u32,
    /// Fill level the controller aims for. Must exceed one capture packet plus one output
    /// period plus scheduling jitter, e.g. 40 ms for 10 ms WASAPI packets.
    pub target_ms: f32,
    /// Ring capacity. Bursts beyond this are dropped.
    pub capacity_ms: f32,
    /// If the fill exceeds the target by more than this, skip straight back to the target
    /// instead of slowly resampling the excess away (e.g. after the output stalled).
    pub resync_ms: f32,
    /// Largest speed correction the controller may apply.
    pub max_correction_ppm: f32,
    /// Time constant of the fill-level average the controller acts on.
    pub smoothing_ms: f32,
    /// Closed-loop time constant: how fast fill errors are corrected.
    pub response_ms: f32,
}

impl BridgeConfig {
    pub fn new(sample_rate: u32, target_ms: f32) -> Self {
        Self {
            sample_rate,
            target_ms,
            capacity_ms: 2000.0,
            resync_ms: (3.0 * target_ms).max(120.0),
            max_correction_ppm: 1000.0,
            smoothing_ms: 1000.0,
            response_ms: 5000.0,
        }
    }

    fn frames(&self, ms: f32) -> f64 {
        f64::from(ms) * f64::from(self.sample_rate) / 1000.0
    }
}

/// Counters readable from any thread while the bridge runs.
#[derive(Debug, Default)]
pub struct BridgeStats {
    fill_frames: AtomicU32,
    correction_ppm: AtomicU32,
    underruns: AtomicU64,
    overruns: AtomicU64,
    resyncs: AtomicU64,
    sample_rate: AtomicU32,
}

impl BridgeStats {
    /// Averaged fill level in frames.
    pub fn fill_frames(&self) -> u32 {
        self.fill_frames.load(Ordering::Relaxed)
    }
    /// Averaged buffering in milliseconds (the latency the bridge adds).
    pub fn fill_ms(&self) -> f32 {
        let sr = self.sample_rate.load(Ordering::Relaxed).max(1);
        self.fill_frames() as f32 * 1000.0 / sr as f32
    }
    /// Current speed correction: positive means the output consumes input faster than nominal
    /// (the capture clock runs fast relative to the output clock).
    pub fn correction_ppm(&self) -> f32 {
        f32::from_bits(self.correction_ppm.load(Ordering::Relaxed))
    }
    /// Times the output found the ring empty.
    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }
    /// Times captured audio was dropped because the ring was full.
    pub fn overruns(&self) -> u64 {
        self.overruns.load(Ordering::Relaxed)
    }
    /// Times the output skipped ahead because the ring was far too full.
    pub fn resyncs(&self) -> u64 {
        self.resyncs.load(Ordering::Relaxed)
    }
    /// All glitches combined, for a single "dropouts" number in the UI.
    pub fn dropouts(&self) -> u64 {
        self.underruns() + self.overruns() + self.resyncs()
    }
}

/// Create a bridge. Give the producer to the capture thread and the consumer to the output
/// callback.
pub fn bridge(config: BridgeConfig) -> (BridgeProducer, BridgeConsumer, Arc<BridgeStats>) {
    assert!(config.sample_rate > 0 && config.target_ms > 0.0);
    let capacity_frames = config.frames(config.capacity_ms).ceil().max(config.frames(config.target_ms) * 2.0) as usize;
    let (tx, rx) = rtrb::RingBuffer::new(capacity_frames * 2);
    let stats = Arc::new(BridgeStats::default());
    stats.sample_rate.store(config.sample_rate, Ordering::Relaxed);
    let target = config.frames(config.target_ms);
    let consumer = BridgeConsumer {
        rx,
        stats: stats.clone(),
        target,
        resync_above: target + config.frames(config.resync_ms),
        max_correction: f64::from(config.max_correction_ppm) * 1e-6,
        gain: 1.0 / (f64::from(config.response_ms) / 1000.0 * f64::from(config.sample_rate)) * target,
        smoothing_frames: config.frames(config.smoothing_ms).max(1.0),
        fill_avg: 0.0,
        hist: [0.0; 8],
        frac: 0.0,
        primed: false,
    };
    (BridgeProducer { tx, stats: stats.clone() }, consumer, stats)
}

/// Capture side.
pub struct BridgeProducer {
    tx: rtrb::Producer<f32>,
    stats: Arc<BridgeStats>,
}

impl BridgeProducer {
    /// Append interleaved stereo frames. Whatever does not fit is dropped and counted as one
    /// overrun.
    pub fn push(&mut self, interleaved: &[f32]) {
        debug_assert!(interleaved.len().is_multiple_of(2), "bridge carries interleaved stereo");
        let room = self.tx.slots() & !1;
        let n = interleaved.len().min(room) & !1;
        if n > 0 {
            // Cannot fail: n <= free slots.
            let _ = self.tx.push_entire_slice(&interleaved[..n]);
        }
        if n < interleaved.len() {
            self.stats.overruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Append `frames` frames of silence (e.g. to fill a detected capture gap and keep timing).
    pub fn push_silence(&mut self, frames: usize) {
        const ZEROS: [f32; 512] = [0.0; 512];
        let mut left = frames * 2;
        while left > 0 {
            let n = left.min(ZEROS.len());
            self.push(&ZEROS[..n]);
            left -= n;
        }
    }

    /// Free space in frames.
    pub fn free_frames(&self) -> usize {
        self.tx.slots() / 2
    }

    pub fn stats(&self) -> &Arc<BridgeStats> {
        &self.stats
    }
}

/// Output side.
pub struct BridgeConsumer {
    rx: rtrb::Consumer<f32>,
    stats: Arc<BridgeStats>,
    target: f64,
    resync_above: f64,
    max_correction: f64,
    /// Proportional gain: correction (fraction of nominal speed) per frame of fill error.
    gain: f64,
    smoothing_frames: f64,
    fill_avg: f64,
    /// Last four input frames, oldest first, interleaved L/R. Output is interpolated between
    /// frames 1 and 2 at position `frac`.
    hist: [f32; 8],
    frac: f64,
    primed: bool,
}

impl BridgeConsumer {
    /// Fill `out` (interleaved stereo) completely. Outputs silence while (re)buffering.
    pub fn pull(&mut self, out: &mut [f32]) {
        debug_assert!(out.len().is_multiple_of(2), "bridge carries interleaved stereo");
        let frames = out.len() / 2;
        if frames == 0 {
            return;
        }
        let avail = (self.rx.slots() / 2) as f64;

        if !self.primed {
            if avail < self.target {
                out.fill(0.0);
                return;
            }
            self.primed = true;
            self.fill_avg = avail;
        }

        if avail > self.resync_above {
            let skip = (avail - self.target) as usize * 2;
            if let Ok(chunk) = self.rx.read_chunk(skip) {
                chunk.commit_all();
            }
            self.fill_avg = self.target;
            self.stats.resyncs.fetch_add(1, Ordering::Relaxed);
        } else {
            let a = (frames as f64 / self.smoothing_frames).min(1.0);
            self.fill_avg += (avail - self.fill_avg) * a;
        }

        let correction =
            ((self.fill_avg - self.target) * self.gain / self.target).clamp(-self.max_correction, self.max_correction);
        let step = 1.0 + correction;
        self.stats.fill_frames.store(self.fill_avg.max(0.0) as u32, Ordering::Relaxed);
        self.stats.correction_ppm.store(((correction * 1e6) as f32).to_bits(), Ordering::Relaxed);

        for (i, o) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            while self.frac >= 1.0 {
                if !self.advance() {
                    // Ran dry: silence the rest and rebuffer to the target before resuming.
                    out[i * 2..].fill(0.0);
                    self.primed = false;
                    self.frac = 0.0;
                    self.hist = [0.0; 8];
                    self.stats.underruns.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                self.frac -= 1.0;
            }
            let t = self.frac as f32;
            let h = &self.hist;
            o[0] = catmull_rom(h[0], h[2], h[4], h[6], t);
            o[1] = catmull_rom(h[1], h[3], h[5], h[7], t);
            self.frac += step;
        }
    }

    /// Drop everything buffered and rebuffer from scratch (e.g. after an output device change).
    pub fn reset(&mut self) {
        let n = self.rx.slots() & !1;
        if let Ok(chunk) = self.rx.read_chunk(n) {
            chunk.commit_all();
        }
        self.primed = false;
        self.frac = 0.0;
        self.hist = [0.0; 8];
    }

    pub fn stats(&self) -> &Arc<BridgeStats> {
        &self.stats
    }

    /// Shift one input frame into the interpolation history. False if the ring is empty.
    #[inline]
    fn advance(&mut self) -> bool {
        if self.rx.slots() < 2 {
            return false;
        }
        let (Ok(l), Ok(r)) = (self.rx.pop(), self.rx.pop()) else {
            return false;
        };
        self.hist.copy_within(2.., 0);
        self.hist[6] = l;
        self.hist[7] = r;
        true
    }
}

/// Catmull-Rom spline through x1 (t = 0) and x2 (t = 1).
#[inline]
fn catmull_rom(x0: f32, x1: f32, x2: f32, x3: f32, t: f32) -> f32 {
    x1 + 0.5 * t * (x2 - x0 + t * (2.0 * x0 - 5.0 * x1 + 4.0 * x2 - x3 + t * (3.0 * (x1 - x2) + x3 - x0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    /// Simulates a capture clock at `drift_ppm` relative to the output clock, delivering
    /// `packet` frames at a time, with the output pulling `period` frames at a time.
    struct Sim {
        prod: BridgeProducer,
        cons: BridgeConsumer,
        stats: Arc<BridgeStats>,
        sr: f64,
        in_rate: f64,
        in_acc: f64,
        out_acc: f64,
        phase: f64,
        packet: usize,
        period: usize,
        out: Vec<f32>,
        last: f32,
        max_jump: f32,
        in_buf: Vec<f32>,
    }

    impl Sim {
        fn new(drift_ppm: f64, packet: usize, period: usize, target_ms: f32) -> Self {
            let sr = 48_000.0;
            let (prod, cons, stats) = bridge(BridgeConfig::new(sr as u32, target_ms));
            Self {
                prod,
                cons,
                stats,
                sr,
                in_rate: sr * (1.0 + drift_ppm * 1e-6),
                in_acc: 0.0,
                out_acc: 0.0,
                phase: 0.0,
                packet,
                period,
                out: vec![0.0; period * 2],
                last: 0.0,
                max_jump: 0.0,
                in_buf: vec![0.0; packet * 2],
            }
        }

        /// Advance simulated time by `secs` in 0.5 ms steps. `pull` = false freezes the output.
        fn run(&mut self, secs: f64, pull: bool) {
            let dt = 0.0005;
            for _ in 0..(secs / dt) as usize {
                self.in_acc += self.in_rate * dt;
                while self.in_acc >= self.packet as f64 {
                    self.in_acc -= self.packet as f64;
                    for f in self.in_buf.as_chunks_mut::<2>().0 {
                        // 440 Hz in the capture clock's own time base.
                        let s = (self.phase * TAU).sin() as f32 * 0.5;
                        self.phase = (self.phase + 440.0 / self.sr).fract();
                        f[0] = s;
                        f[1] = -s;
                    }
                    self.prod.push(&self.in_buf);
                }
                if !pull {
                    continue;
                }
                self.out_acc += self.sr * dt;
                while self.out_acc >= self.period as f64 {
                    self.out_acc -= self.period as f64;
                    self.cons.pull(&mut self.out);
                    for f in self.out.as_chunks::<2>().0 {
                        assert!((f[0] + f[1]).abs() < 1e-6, "channels must stay paired");
                        self.max_jump = self.max_jump.max((f[0] - self.last).abs());
                        self.last = f[0];
                    }
                }
            }
        }
    }

    fn check_drift(drift_ppm: f64) {
        let mut sim = Sim::new(drift_ppm, 480, 128, 40.0);
        sim.run(30.0, true); // settle
        let glitches = sim.stats.dropouts();
        sim.max_jump = 0.0;
        sim.run(120.0, true);
        assert_eq!(sim.stats.dropouts(), glitches, "no glitches once settled (drift {drift_ppm} ppm)");
        let ppm = sim.stats.correction_ppm() as f64;
        assert!((ppm - drift_ppm).abs() < 40.0, "correction {ppm} ppm should track drift {drift_ppm} ppm");
        let fill = sim.stats.fill_ms();
        assert!((30.0..60.0).contains(&fill), "fill {fill} ms should sit near the 40 ms target");
        // A 0.5-amplitude 440 Hz sine moves at most 0.5 * 2*pi*440/48000 = 0.0288 per sample.
        assert!(sim.max_jump < 0.031, "output must stay continuous, max jump {}", sim.max_jump);
    }

    #[test]
    fn tracks_fast_capture_clock() {
        check_drift(350.0);
    }

    #[test]
    fn tracks_slow_capture_clock() {
        check_drift(-500.0);
    }

    #[test]
    fn tracks_equal_clocks() {
        check_drift(0.0);
    }

    #[test]
    fn resyncs_after_output_stall() {
        let mut sim = Sim::new(0.0, 480, 128, 40.0);
        sim.run(5.0, true);
        sim.run(0.5, false); // output stalls for 500 ms, capture keeps coming
        sim.run(0.05, true);
        assert!(sim.stats.resyncs() >= 1);
        sim.run(5.0, true);
        let fill = sim.stats.fill_ms();
        assert!((30.0..60.0).contains(&fill), "fill {fill} ms back near target after resync");
    }

    #[test]
    fn underrun_outputs_silence_then_recovers() {
        let (mut prod, mut cons, stats) = bridge(BridgeConfig::new(48_000, 10.0));
        prod.push(&vec![0.25; 480 * 2 * 2]); // 20 ms
        let mut out = vec![1.0; 256 * 2];
        for _ in 0..10 {
            cons.pull(&mut out);
        }
        assert!(stats.underruns() >= 1);
        assert!(out.iter().all(|&s| s == 0.0), "silence while rebuffering");
        prod.push(&vec![0.25; 480 * 2 * 2]);
        cons.pull(&mut out);
        cons.pull(&mut out);
        assert!((out[out.len() - 1] - 0.25).abs() < 1e-6, "audio resumes after rebuffering");
    }

    #[test]
    fn overrun_drops_whole_frames() {
        let (mut prod, mut cons, stats) = bridge(BridgeConfig { capacity_ms: 1.0, ..BridgeConfig::new(48_000, 1.0) });
        let mut data = Vec::new();
        for i in 0..1000 {
            data.push(i as f32);
            data.push(-(i as f32));
        }
        prod.push(&data);
        assert_eq!(stats.overruns(), 1);
        let mut out = vec![0.0; 64];
        cons.pull(&mut out);
        for f in out.as_chunks::<2>().0 {
            assert_eq!(f[0], -f[1]);
        }
    }

    #[test]
    fn catmull_rom_hits_knots_and_lines() {
        assert_eq!(catmull_rom(0.0, 1.0, 2.0, 3.0, 0.0), 1.0);
        assert_eq!(catmull_rom(0.0, 1.0, 2.0, 3.0, 1.0), 2.0);
        assert!((catmull_rom(0.0, 1.0, 2.0, 3.0, 0.25) - 1.25).abs() < 1e-6);
    }
}
