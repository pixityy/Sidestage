//! The real-time half of the IOProcs, independent of Core Audio (unit-tested on every OS).
//!
//! An aggregate device's IOProc gets one `AudioBuffer` per stream, each interleaving that
//! stream's channels. Devices differ: one stereo stream, one 8-channel stream, or eight mono
//! streams (which is what "non-interleaved" looks like at the HAL). So channels are addressed by
//! their device-wide number: the app's stereo pair is gathered from the tap streams into an
//! interleaved scratch buffer, the [`Processor`] runs in chunks of at most
//! [`MAX_BLOCK_FRAMES`], and the result is scattered to the output's stereo pair. Every other
//! output channel keeps the silence the HAL pre-filled.
//!
//! This runs on the IO thread: no allocation, locks, syscalls, logging or panics (every index
//! is checked).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::{AtomicLevel, MAX_BLOCK_FRAMES, Processor, peak};

/// Read access to an IOProc buffer list.
pub trait Buffers {
    fn count(&self) -> usize;
    /// Interleaved channels in buffer `i`.
    fn channels(&self, i: usize) -> usize;
    /// Samples of buffer `i`, or `None` for a disabled stream (NULL data).
    fn samples(&self, i: usize) -> Option<&[f32]>;
}

/// Write access to an IOProc's output buffer list.
pub trait BuffersMut: Buffers {
    fn samples_mut(&mut self, i: usize) -> Option<&mut [f32]>;
}

/// Where one device channel lives in a buffer list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    buffer: usize,
    offset: usize,
    stride: usize,
}

/// Finds channel `channel` (0-based, counted across the buffers from `first` on).
pub fn locate<B: Buffers + ?Sized>(list: &B, first: usize, channel: usize) -> Option<Slot> {
    let mut base = 0usize;
    for buffer in first..list.count() {
        let stride = list.channels(buffer);
        if channel < base + stride {
            return Some(Slot { buffer, offset: channel - base, stride });
        }
        base += stride;
    }
    None
}

fn frames_in<B: Buffers + ?Sized>(list: &B, slot: Slot) -> usize {
    list.samples(slot.buffer).map_or(0, |s| s.len() / slot.stride.max(1))
}

/// Counters the render IOProc publishes for the control thread.
#[derive(Debug, Default)]
pub struct RenderMeters {
    /// Peak of the captured music since the UI last looked.
    pub capture: AtomicLevel,
    /// IO cycles that did not continue the previous one seamlessly (overloads, device hiccups).
    pub dropouts: AtomicU64,
    /// Consecutive frames of exact digital silence from the tap.
    pub silent_frames: AtomicU64,
    /// Frames between a cycle's input (capture) and output timestamps.
    pub latency_frames: AtomicU32,
    /// IO cycles so far.
    pub cycles: AtomicU64,
}

/// The sidechain level ("everything except the music").
#[derive(Debug, Default)]
pub struct SidechainLevel {
    /// Peak of the sidechain's latest IO cycle, for the ducker.
    pub now: AtomicLevel,
    /// Peak since the UI last looked.
    pub meter: AtomicLevel,
    /// Bumped every sidechain cycle, so the renderer can tell a live value from a stale one.
    pub seq: AtomicU32,
}

/// Sidechain IOProc body: the peak over every tap channel.
pub fn meter_sidechain<B: Buffers + ?Sized>(level: &SidechainLevel, input: &B) {
    let mut p = 0.0f32;
    for i in 0..input.count() {
        if let Some(s) = input.samples(i) {
            p = p.max(peak(s));
        }
    }
    level.now.store(p);
    level.meter.raise(p);
    level.seq.fetch_add(1, Ordering::Relaxed);
}

/// Which channels to read and write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StereoMap {
    /// Input buffers belonging to the tap. They are the last ones; the output device's own
    /// input streams (if any) come before them. Counting from the end, as FineTune does, keeps
    /// the tap found even if the HAL ever left disabled streams out of the list.
    pub tap_buffers: usize,
    /// Tap channels with the app's left and right, counted from the tap's first buffer.
    pub tap_left: usize,
    pub tap_right: usize,
    /// Output channels (device-wide, 0-based) for the rendered left and right; equal on a mono
    /// device, which then gets the fold-down.
    pub out_left: usize,
    pub out_right: usize,
}

/// Everything the render IOProc touches. Built on the control thread, then owned by the IO
/// thread until the IOProc is destroyed.
pub struct RenderCore {
    map: StereoMap,
    processor: Box<dyn Processor>,
    input: Box<[f32]>,
    output: Box<[f32]>,
    meters: Arc<RenderMeters>,
    sidechain: Option<Arc<SidechainLevel>>,
    sc_seq: u32,
    sc_stale_frames: usize,
    sc_stale_limit: usize,
    next_sample_time: Option<f64>,
    silent_frames: u64,
}

impl RenderCore {
    pub fn new(
        map: StereoMap,
        processor: Box<dyn Processor>,
        sample_rate: u32,
        meters: Arc<RenderMeters>,
        sidechain: Option<Arc<SidechainLevel>>,
    ) -> Self {
        // The meters outlive engines (rebuilds); what they hold describes the previous one.
        meters.silent_frames.store(0, Ordering::Relaxed);
        meters.latency_frames.store(0, Ordering::Relaxed);
        Self {
            map,
            processor,
            input: vec![0.0; MAX_BLOCK_FRAMES * 2].into_boxed_slice(),
            output: vec![0.0; MAX_BLOCK_FRAMES * 2].into_boxed_slice(),
            meters,
            // Only an update after this point is fresh: the level may be left over from a
            // sidechain that a rebuild just closed.
            sc_seq: sidechain.as_ref().map_or(0, |s| s.seq.load(Ordering::Relaxed)),
            sidechain,
            sc_stale_frames: usize::MAX,
            // A sidechain that has not updated for 250 ms is gone (or stuck): stop ducking.
            sc_stale_limit: sample_rate as usize / 4,
            next_sample_time: None,
            silent_frames: 0,
        }
    }

    /// One IO cycle. `input_time`/`output_time` are the cycle's sample times when valid.
    pub fn render<I: Buffers + ?Sized, O: BuffersMut + ?Sized>(
        &mut self,
        input: &I,
        output: &mut O,
        input_time: Option<f64>,
        output_time: Option<f64>,
    ) {
        self.meters.cycles.fetch_add(1, Ordering::Relaxed);
        let Some(out_l) = locate(output, 0, self.map.out_left) else {
            return;
        };
        let out_r = locate(output, 0, self.map.out_right).unwrap_or(out_l);
        let frames = frames_in(output, out_l).min(frames_in(output, out_r));

        if let Some(t) = output_time {
            if self.next_sample_time.is_some_and(|expected| (t - expected).abs() >= 1.0) {
                self.meters.dropouts.fetch_add(1, Ordering::Relaxed);
            }
            self.next_sample_time = Some(t + frames as f64);
            if let Some(it) = input_time
                && t >= it
            {
                self.meters.latency_frames.store((t - it).min(f64::from(u32::MAX)) as u32, Ordering::Relaxed);
            }
        }

        let first = input.count().saturating_sub(self.map.tap_buffers);
        let in_l = locate(input, first, self.map.tap_left);
        let in_r = locate(input, first, self.map.tap_right);
        let sidechain = self.sidechain_peak(frames);

        let mut cycle_peak = 0.0f32;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_BLOCK_FRAMES);
            let (Some(inp), Some(out)) = (self.input.get_mut(..n * 2), self.output.get_mut(..n * 2)) else {
                return;
            };
            gather(input, in_l, done, inp, 0);
            gather(input, in_r, done, inp, 1);
            let p = peak(inp);
            cycle_peak = cycle_peak.max(p);
            self.silent_frames = if p == 0.0 { self.silent_frames.saturating_add(n as u64) } else { 0 };
            self.processor.process(inp, out, sidechain);
            scatter(output, out_l, out_r, done, out);
            done += n;
        }
        self.meters.capture.raise(cycle_peak);
        self.meters.silent_frames.store(self.silent_frames, Ordering::Relaxed);
    }

    fn sidechain_peak(&mut self, frames: usize) -> f32 {
        let Some(sc) = &self.sidechain else {
            return 0.0;
        };
        let seq = sc.seq.load(Ordering::Relaxed);
        if seq != self.sc_seq {
            self.sc_seq = seq;
            self.sc_stale_frames = 0;
        } else {
            self.sc_stale_frames = self.sc_stale_frames.saturating_add(frames);
        }
        if self.sc_stale_frames > self.sc_stale_limit { 0.0 } else { sc.now.load() }
    }
}

/// Copies one side (0 = left, 1 = right) of the app's stereo into interleaved `dst`. A missing
/// or disabled stream reads as silence; NaN and infinities from a misbehaving app read as 0.
fn gather<B: Buffers + ?Sized>(list: &B, slot: Option<Slot>, start: usize, dst: &mut [f32], side: usize) {
    let src = slot.and_then(|s| list.samples(s.buffer).map(|data| (s, data)));
    for (f, frame) in dst.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let v = match src {
            Some((s, data)) => data.get((start + f) * s.stride + s.offset).copied().unwrap_or(0.0),
            None => 0.0,
        };
        if let Some(x) = frame.get_mut(side) {
            *x = if v.is_finite() { v } else { 0.0 };
        }
    }
}

/// Writes interleaved stereo `src` to the output pair (folded to mono when both are one channel).
fn scatter<O: BuffersMut + ?Sized>(list: &mut O, l: Slot, r: Slot, start: usize, src: &[f32]) {
    if l == r {
        let Some(data) = list.samples_mut(l.buffer) else {
            return;
        };
        for (f, [a, b]) in src.as_chunks::<2>().0.iter().enumerate() {
            if let Some(x) = data.get_mut((start + f) * l.stride + l.offset) {
                *x = 0.5 * (a + b);
            }
        }
    } else {
        write_side(list, l, start, src, 0);
        write_side(list, r, start, src, 1);
    }
}

fn write_side<O: BuffersMut + ?Sized>(list: &mut O, slot: Slot, start: usize, src: &[f32], side: usize) {
    let Some(data) = list.samples_mut(slot.buffer) else {
        return;
    };
    for (f, frame) in src.as_chunks::<2>().0.iter().enumerate() {
        if let (Some(x), Some(&v)) = (data.get_mut((start + f) * slot.stride + slot.offset), frame.get(side)) {
            *x = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Buffer list in the HAL's shape: per stream, its channel count and (unless disabled) data.
    struct Bufs(Vec<(usize, Option<Vec<f32>>)>);

    impl Buffers for Bufs {
        fn count(&self) -> usize {
            self.0.len()
        }
        fn channels(&self, i: usize) -> usize {
            self.0.get(i).map_or(0, |b| b.0)
        }
        fn samples(&self, i: usize) -> Option<&[f32]> {
            self.0.get(i)?.1.as_deref()
        }
    }

    impl BuffersMut for Bufs {
        fn samples_mut(&mut self, i: usize) -> Option<&mut [f32]> {
            self.0.get_mut(i)?.1.as_deref_mut()
        }
    }

    /// Passes input through (times `gain`) and records the largest block and the sidechain.
    struct Pass {
        gain: f32,
        max_block: Arc<AtomicUsize>,
        sidechain: Arc<AtomicLevel>,
    }

    impl Processor for Pass {
        fn process(&mut self, input: &[f32], output: &mut [f32], sidechain_peak: f32) {
            assert_eq!(input.len(), output.len());
            assert!(input.len() <= MAX_BLOCK_FRAMES * 2);
            self.max_block.fetch_max(input.len() / 2, Ordering::Relaxed);
            self.sidechain.store(sidechain_peak);
            for (o, i) in output.iter_mut().zip(input) {
                *o = i * self.gain;
            }
        }
    }

    struct Rig {
        core: RenderCore,
        meters: Arc<RenderMeters>,
        max_block: Arc<AtomicUsize>,
        sidechain_seen: Arc<AtomicLevel>,
    }

    fn rig(map: StereoMap, sidechain: Option<Arc<SidechainLevel>>) -> Rig {
        let meters = Arc::new(RenderMeters::default());
        let max_block = Arc::new(AtomicUsize::new(0));
        let sidechain_seen = Arc::new(AtomicLevel::new());
        let pass = Pass { gain: 1.0, max_block: max_block.clone(), sidechain: sidechain_seen.clone() };
        let core = RenderCore::new(map, Box::new(pass), 48_000, meters.clone(), sidechain);
        Rig { core, meters, max_block, sidechain_seen }
    }

    const STEREO: StereoMap = StereoMap { tap_buffers: 1, tap_left: 0, tap_right: 1, out_left: 0, out_right: 1 };

    fn ramp(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames * channels).map(|i| (i + 1) as f32 / 1000.0).collect()
    }

    #[test]
    fn interleaved_stereo_passes_through() {
        let mut r = rig(STEREO, None);
        let input = Bufs(vec![(2, Some(ramp(256, 2)))]);
        let mut output = Bufs(vec![(2, Some(vec![0.0; 512]))]);
        r.core.render(&input, &mut output, None, None);
        assert_eq!(output.0[0].1.as_deref(), input.0[0].1.as_deref());
        assert!((r.meters.capture.take() - 0.512).abs() < 1e-6);
        assert_eq!(r.meters.silent_frames.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn skips_device_inputs_and_writes_non_interleaved_outputs() {
        // Input: the headset mic stream (disabled), then the stereo tap.
        let tap = ramp(64, 2);
        let input = Bufs(vec![(1, None), (2, Some(tap.clone()))]);
        // Output: four mono streams; the preferred stereo pair is channels 3/4.
        let mut output = Bufs((0..4).map(|_| (1, Some(vec![0.0; 64]))).collect());
        let map = StereoMap { tap_buffers: 1, tap_left: 0, tap_right: 1, out_left: 2, out_right: 3 };
        let mut r = rig(map, None);
        r.core.render(&input, &mut output, None, None);
        let left: Vec<f32> = tap.iter().step_by(2).copied().collect();
        let right: Vec<f32> = tap.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(output.0[2].1.as_deref(), Some(&left[..]));
        assert_eq!(output.0[3].1.as_deref(), Some(&right[..]));
        assert!(output.0[0].1.as_ref().is_some_and(|b| b.iter().all(|&s| s == 0.0)));
        assert!(output.0[1].1.as_ref().is_some_and(|b| b.iter().all(|&s| s == 0.0)));
    }

    #[test]
    fn tap_is_found_from_the_end_of_the_input_list() {
        let tap = ramp(32, 2);
        let map = STEREO;
        // A live headset microphone (its streams could not be turned off) must not be read.
        let mic = vec![0.9; 32];
        let with_mic = Bufs(vec![(1, Some(mic.clone())), (1, Some(mic)), (2, Some(tap.clone()))]);
        // The same cycle if the HAL left the disabled microphone streams out of the list.
        let without_mic = Bufs(vec![(2, Some(tap.clone()))]);
        for input in [with_mic, without_mic] {
            let mut output = Bufs(vec![(2, Some(vec![0.0; 64]))]);
            let mut r = rig(map, None);
            r.core.render(&input, &mut output, None, None);
            assert_eq!(output.0[0].1.as_deref(), Some(&tap[..]));
        }
        // A tap delivered as two mono buffers after the microphone.
        let left: Vec<f32> = tap.iter().step_by(2).copied().collect();
        let right: Vec<f32> = tap.iter().skip(1).step_by(2).copied().collect();
        let split = Bufs(vec![(1, None), (1, Some(left)), (1, Some(right))]);
        let mut output = Bufs(vec![(2, Some(vec![0.0; 64]))]);
        let mut r = rig(StereoMap { tap_buffers: 2, ..map }, None);
        r.core.render(&split, &mut output, None, None);
        assert_eq!(output.0[0].1.as_deref(), Some(&tap[..]));
    }

    #[test]
    fn multichannel_tap_and_output() {
        // 8-channel interface: the app's stereo is on channels 3/4 of the tap; we render to 3/4.
        let frames = 32;
        let mut tap = vec![0.0; frames * 8];
        for f in 0..frames {
            tap[f * 8 + 2] = 0.25;
            tap[f * 8 + 3] = -0.5;
        }
        let input = Bufs(vec![(8, Some(tap))]);
        let mut output = Bufs(vec![(8, Some(vec![0.0; frames * 8]))]);
        let map = StereoMap { tap_buffers: 1, tap_left: 2, tap_right: 3, out_left: 2, out_right: 3 };
        let mut r = rig(map, None);
        r.core.render(&input, &mut output, None, None);
        let out = output.0[0].1.as_deref().unwrap();
        for f in 0..frames {
            assert_eq!(&out[f * 8..f * 8 + 8], &[0.0, 0.0, 0.25, -0.5, 0.0, 0.0, 0.0, 0.0]);
        }
    }

    #[test]
    fn mono_tap_and_mono_output() {
        let input = Bufs(vec![(1, Some(vec![0.5; 16]))]);
        let mut output = Bufs(vec![(1, Some(vec![0.0; 16]))]);
        let map = StereoMap { tap_buffers: 1, tap_left: 0, tap_right: 0, out_left: 0, out_right: 0 };
        let mut r = rig(map, None);
        r.core.render(&input, &mut output, None, None);
        assert!(output.0[0].1.as_ref().unwrap().iter().all(|&s| s == 0.5));
    }

    #[test]
    fn large_buffers_are_chunked() {
        let frames = MAX_BLOCK_FRAMES * 2 + 100;
        let input = Bufs(vec![(2, Some(ramp(frames, 2)))]);
        let mut output = Bufs(vec![(2, Some(vec![0.0; frames * 2]))]);
        let mut r = rig(STEREO, None);
        r.core.render(&input, &mut output, None, None);
        assert_eq!(r.max_block.load(Ordering::Relaxed), MAX_BLOCK_FRAMES);
        assert_eq!(output.0[0].1.as_deref(), input.0[0].1.as_deref(), "chunks line up seamlessly");
    }

    #[test]
    fn disabled_tap_reads_as_silence_and_is_counted() {
        let input = Bufs(vec![(2, None)]);
        let mut output = Bufs(vec![(2, Some(vec![1.0; 256]))]);
        let mut r = rig(STEREO, None);
        r.core.render(&input, &mut output, None, None);
        r.core.render(&input, &mut output, None, None);
        assert!(output.0[0].1.as_ref().unwrap().iter().all(|&s| s == 0.0));
        assert_eq!(r.meters.silent_frames.load(Ordering::Relaxed), 256);
        let loud = Bufs(vec![(2, Some(vec![0.1; 256]))]);
        r.core.render(&loud, &mut output, None, None);
        assert_eq!(r.meters.silent_frames.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn garbage_samples_are_sanitized_and_short_input_is_padded() {
        let input = Bufs(vec![(2, Some(vec![f32::NAN, f32::INFINITY, 0.25, 0.25]))]);
        let mut output = Bufs(vec![(2, Some(vec![9.0; 8]))]);
        let mut r = rig(STEREO, None);
        r.core.render(&input, &mut output, None, None);
        assert_eq!(output.0[0].1.as_deref(), Some(&[0.0, 0.0, 0.25, 0.25, 0.0, 0.0, 0.0, 0.0][..]));
    }

    #[test]
    fn missing_output_pair_is_a_no_op() {
        let input = Bufs(vec![(2, Some(vec![0.5; 8]))]);
        let mut output = Bufs(vec![]);
        let mut r = rig(STEREO, None);
        r.core.render(&input, &mut output, None, None);
        assert_eq!(r.meters.cycles.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn timestamp_gaps_count_as_dropouts() {
        let input = Bufs(vec![(2, Some(vec![0.0; 1024]))]);
        let mut output = Bufs(vec![(2, Some(vec![0.0; 1024]))]);
        let mut r = rig(STEREO, None);
        r.core.render(&input, &mut output, Some(0.0), Some(1000.0));
        r.core.render(&input, &mut output, Some(512.0), Some(1512.0));
        assert_eq!(r.meters.dropouts.load(Ordering::Relaxed), 0);
        r.core.render(&input, &mut output, Some(4096.0), Some(5096.0));
        assert_eq!(r.meters.dropouts.load(Ordering::Relaxed), 1);
        assert_eq!(r.meters.latency_frames.load(Ordering::Relaxed), 1000);
    }

    #[test]
    fn stale_sidechain_stops_ducking() {
        let sc = Arc::new(SidechainLevel::default());
        let mut r = rig(STEREO, Some(sc.clone()));
        let input = Bufs(vec![(2, Some(vec![0.0; 1024]))]);
        let mut output = Bufs(vec![(2, Some(vec![0.0; 1024]))]);
        meter_sidechain(&sc, &Bufs(vec![(2, Some(vec![0.0, -0.8, 0.3, 0.1]))]));
        assert_eq!(sc.meter.take(), 0.8);
        r.core.render(&input, &mut output, None, None);
        assert_eq!(r.sidechain_seen.load(), 0.8);
        // 12000 frames (250 ms at 48 kHz) of no sidechain updates: treated as gone.
        for _ in 0..30 {
            r.core.render(&input, &mut output, None, None);
        }
        assert_eq!(r.sidechain_seen.load(), 0.0);
        meter_sidechain(&sc, &Bufs(vec![(2, Some(vec![0.4, 0.0]))]));
        r.core.render(&input, &mut output, None, None);
        assert_eq!(r.sidechain_seen.load(), 0.4);
    }

    #[test]
    fn a_rebuilt_core_starts_from_fresh_meters() {
        let sc = Arc::new(SidechainLevel::default());
        // Left over from the previous engine and its (now closed) sidechain.
        meter_sidechain(&sc, &Bufs(vec![(2, Some(vec![0.7, 0.7]))]));
        let meters = Arc::new(RenderMeters::default());
        meters.silent_frames.store(48_000 * 60, Ordering::Relaxed);
        meters.latency_frames.store(4096, Ordering::Relaxed);
        let seen = Arc::new(AtomicLevel::new());
        let pass = Pass { gain: 1.0, max_block: Arc::new(AtomicUsize::new(0)), sidechain: seen.clone() };
        let mut core = RenderCore::new(STEREO, Box::new(pass), 48_000, meters.clone(), Some(sc.clone()));
        assert_eq!(meters.silent_frames.load(Ordering::Relaxed), 0);
        assert_eq!(meters.latency_frames.load(Ordering::Relaxed), 0);
        let input = Bufs(vec![(2, Some(vec![0.1; 256]))]);
        let mut output = Bufs(vec![(2, Some(vec![0.0; 256]))]);
        core.render(&input, &mut output, None, None);
        assert_eq!(seen.load(), 0.0, "no ducking from the old sidechain's level");
        meter_sidechain(&sc, &Bufs(vec![(2, Some(vec![0.2, 0.0]))]));
        core.render(&input, &mut output, None, None);
        assert_eq!(seen.load(), 0.2);
    }

    #[test]
    fn locate_counts_channels_across_buffers() {
        let list = Bufs(vec![(2, None), (1, None), (4, None)]);
        assert_eq!(locate(&list, 0, 0), Some(Slot { buffer: 0, offset: 0, stride: 2 }));
        assert_eq!(locate(&list, 0, 2), Some(Slot { buffer: 1, offset: 0, stride: 1 }));
        assert_eq!(locate(&list, 0, 5), Some(Slot { buffer: 2, offset: 2, stride: 4 }));
        assert_eq!(locate(&list, 1, 1), Some(Slot { buffer: 2, offset: 0, stride: 4 }));
        assert_eq!(locate(&list, 0, 7), None);
        assert_eq!(locate(&list, 9, 0), None);
    }
}
