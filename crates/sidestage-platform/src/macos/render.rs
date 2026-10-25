//! The real-time half of the IOProcs, independent of Core Audio (unit-tested on every OS).
//!
//! A session's aggregate device holds one process tap per source. Its IOProc gets one
//! `AudioBuffer` per input stream: the output device's own input streams (a headset microphone)
//! first, then the taps' streams, each interleaving its channels. Every source is a [`Lane`]
//! that knows where its tap sits counted from the end of that list (FineTune counts the same
//! way, which also keeps the taps found if the HAL ever left disabled device streams out). Per
//! block of at most [`MAX_BLOCK_FRAMES`] every lane's stereo is gathered into an interleaved
//! scratch buffer and run through its own [`Processor`], the outputs are summed, the master
//! processor runs on the sum, and the result is scattered to the output's stereo pair. Every
//! other output channel keeps the silence the HAL pre-filled.
//!
//! **Handing processors between engines.** Changing the sources rebuilds the aggregate (see
//! `engine.rs` for why), but a source that stays must keep its processor, with its reverb tail
//! and smoothing state. Each processor lives in a [`DspCell`] that successive engines share. An
//! IO cycle takes a processor out of its cell with one atomic swap and puts it back with a
//! store, so at most one IO thread ever runs it, without locks and whatever the two engines'
//! IO threads do; a cycle that finds the cell empty skips that source (a counted gap). Which
//! engine renders is decided by [`Handover`]: the control thread pauses the old engine, which
//! fades out in its next cycle and reports that it let go, then activates the new one, which
//! fades in.
//!
//! This runs on the IO thread: no allocation, locks, syscalls, logging or panics (every index
//! is checked).

use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

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

/// Where one channel lives in a buffer list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    buffer: usize,
    offset: usize,
    stride: usize,
}

/// Finds channel `channel` (0-based, counted across buffers `first..end`).
pub fn locate<B: Buffers + ?Sized>(list: &B, first: usize, end: usize, channel: usize) -> Option<Slot> {
    let mut base = 0usize;
    for buffer in first..end.min(list.count()) {
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

/// Generation number that no engine has: while it is active, nobody renders.
pub const IDLE: u32 = 0;

/// The generation after `current`; never [`IDLE`].
pub fn next_generation(current: u32) -> u32 {
    match current.wrapping_add(1) {
        IDLE => 1,
        g => g,
    }
}

/// Which engine generation may render. Generations start at 1.
#[derive(Debug, Default)]
pub struct Handover {
    active: AtomicU32,
    /// The last generation that finished its fade-out and put its processors back.
    retired: AtomicU32,
}

impl Handover {
    pub fn activate(&self, generation: u32) {
        self.active.store(generation, Ordering::Release);
    }

    /// Asks the active engine to fade out and let go of its processors.
    pub fn pause(&self) {
        self.active.store(IDLE, Ordering::Release);
    }

    pub fn has_retired(&self, generation: u32) -> bool {
        self.retired.load(Ordering::Acquire) == generation
    }
}

struct Dsp(Box<dyn Processor>);

// `AtomicPtr` is `Sync` for any pointee; the processor it hands between IO threads must be
// `Send`, which `Processor` requires. This keeps that true if `Dsp` ever changes.
const _: fn() = || {
    fn send<T: Send>() {}
    send::<Dsp>();
};

/// One processor, shared by the engines that render it in turn (see the module docs).
#[derive(Debug)]
pub struct DspCell {
    slot: AtomicPtr<Dsp>,
    rate: u32,
}

impl DspCell {
    /// `rate` is the sample rate the processor was built for.
    pub fn new(processor: Box<dyn Processor>, rate: u32) -> Self {
        Self { slot: AtomicPtr::new(Box::into_raw(Box::new(Dsp(processor)))), rate }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Exclusive use of the processor until the guard drops, or `None` while another IO
    /// thread has it. Wait-free.
    fn take(&self) -> Option<DspGuard<'_>> {
        let dsp = NonNull::new(self.slot.swap(ptr::null_mut(), Ordering::Acquire))?;
        Some(DspGuard { cell: self, dsp })
    }
}

impl Drop for DspCell {
    fn drop(&mut self) {
        // Empty only if an IO thread that Core Audio failed to stop still holds it: leak it.
        if let Some(dsp) = NonNull::new(*self.slot.get_mut()) {
            // SAFETY: the pointer came from `Box::into_raw` and nobody else can reach it now.
            drop(unsafe { Box::from_raw(dsp.as_ptr()) });
        }
    }
}

struct DspGuard<'a> {
    cell: &'a DspCell,
    dsp: NonNull<Dsp>,
}

impl DspGuard<'_> {
    fn process(&mut self, input: &[f32], output: &mut [f32], sidechain_peak: f32) {
        // SAFETY: the swap in `take` made this thread the only user until the guard drops.
        unsafe { self.dsp.as_mut() }.0.process(input, output, sidechain_peak);
    }
}

impl Drop for DspGuard<'_> {
    fn drop(&mut self) {
        self.cell.slot.store(self.dsp.as_ptr(), Ordering::Release);
    }
}

/// Counters of one source, published by whichever engine renders it.
#[derive(Debug, Default)]
pub struct SourceMeters {
    /// Peak of the captured audio since the UI last looked.
    pub capture: AtomicLevel,
    /// Consecutive frames of exact digital silence from the tap.
    pub silent_frames: AtomicU64,
    /// IO cycles in which the source was skipped because the previous engine still had its
    /// processor (during a rebuild).
    pub gaps: AtomicU64,
}

/// Counters of the session's output, kept across engines.
#[derive(Debug, Default)]
pub struct SessionMeters {
    /// IO cycles that did not continue the previous one seamlessly (overloads, device hiccups).
    pub dropouts: AtomicU64,
    /// Frames between a cycle's input (capture) and output timestamps.
    pub latency_frames: AtomicU32,
}

/// Counters of one engine.
#[derive(Debug, Default)]
pub struct EngineMeters {
    /// IO cycles so far, live or not.
    pub cycles: AtomicU64,
    /// Cycles whose input buffers did not have the layout the engine was built for.
    pub layout_errors: AtomicU64,
}

/// The sidechain level ("everything except the sources, the ignored apps and Sidestage").
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

/// Where one source's tap sits in the input buffer list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneMap {
    /// The tap's first buffer, counted from the end of the list (1 = the last buffer).
    pub from_end: usize,
    /// Channels of each of the tap's buffers, as built; a cycle with another layout reads the
    /// source as silence.
    pub channels: Box<[usize]>,
    /// Tap channels with the app's left and right, counted from the tap's first buffer.
    pub left: usize,
    pub right: usize,
}

/// One source as the IO thread sees it.
pub struct LaneSetup {
    pub map: LaneMap,
    pub dsp: Arc<DspCell>,
    pub meters: Arc<SourceMeters>,
}

/// Everything a [`RenderCore`] is built from.
pub struct CoreSetup {
    pub generation: u32,
    pub handover: Arc<Handover>,
    pub lanes: Vec<LaneSetup>,
    pub master: Arc<DspCell>,
    /// Output channels (device-wide, 0-based) for the rendered left and right; equal on a mono
    /// device, which then gets the fold-down.
    pub out_left: usize,
    pub out_right: usize,
    pub sample_rate: u32,
    pub session: Arc<SessionMeters>,
    pub engine: Arc<EngineMeters>,
    pub sidechain: Option<Arc<SidechainLevel>>,
}

struct Lane {
    map: LaneMap,
    dsp: Arc<DspCell>,
    meters: Arc<SourceMeters>,
    /// This cycle's slots for the app's left and right; `None` when the layout is off.
    slots: Option<(Slot, Slot)>,
    silent: u64,
    peak: f32,
    skipped: bool,
}

/// Everything the render IOProc touches. Built on the control thread, then owned by the IO
/// thread until the IOProc is destroyed. Its `Arc`s are only dropped there, never on the IO
/// thread.
pub struct RenderCore {
    generation: u32,
    handover: Arc<Handover>,
    lanes: Box<[Lane]>,
    master: Arc<DspCell>,
    out_left: usize,
    out_right: usize,
    session: Arc<SessionMeters>,
    engine: Arc<EngineMeters>,
    sidechain: Option<Arc<SidechainLevel>>,
    sc_seq: u32,
    sc_stale_frames: usize,
    sc_stale_limit: usize,
    input: Box<[f32]>,
    output: Box<[f32]>,
    mix: Box<[f32]>,
    out: Box<[f32]>,
    /// Rendering (active, or fading out in its last cycle).
    live: bool,
    /// Frames of the fade-in done since this engine went live.
    faded_in: usize,
    fade_len: usize,
    next_sample_time: Option<f64>,
}

/// Fade-in when an engine takes over, so a rebuild dips instead of clicking.
const FADE_SECONDS: f64 = 0.005;

impl RenderCore {
    pub fn new(setup: CoreSetup) -> Self {
        let lanes = setup
            .lanes
            .into_iter()
            .map(|l| Lane {
                map: l.map,
                dsp: l.dsp,
                meters: l.meters,
                slots: None,
                silent: 0,
                peak: 0.0,
                skipped: false,
            })
            .collect();
        let scratch = || vec![0.0; MAX_BLOCK_FRAMES * 2].into_boxed_slice();
        Self {
            generation: setup.generation,
            handover: setup.handover,
            lanes,
            master: setup.master,
            out_left: setup.out_left,
            out_right: setup.out_right,
            session: setup.session,
            engine: setup.engine,
            // Only an update after this point is fresh: the level may be left over from a
            // sidechain that has since closed.
            sc_seq: setup.sidechain.as_ref().map_or(0, |s| s.seq.load(Ordering::Relaxed)),
            sidechain: setup.sidechain,
            sc_stale_frames: usize::MAX,
            // A sidechain that has not updated for 250 ms is gone (or stuck): stop ducking.
            sc_stale_limit: setup.sample_rate as usize / 4,
            input: scratch(),
            output: scratch(),
            mix: scratch(),
            out: scratch(),
            live: false,
            faded_in: 0,
            fade_len: ((f64::from(setup.sample_rate) * FADE_SECONDS) as usize).max(1),
            next_sample_time: None,
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
        self.engine.cycles.fetch_add(1, Ordering::Relaxed);
        let active = self.handover.active.load(Ordering::Acquire) == self.generation;
        if !active && !self.live {
            // Standby (warming up) or retired: the HAL's silence goes out, the taps are still
            // read, so their apps stay muted.
            return;
        }
        if !self.live {
            self.live = true;
            self.faded_in = 0;
            self.next_sample_time = None;
        }
        let retiring = !active;
        self.render_live(input, output, input_time, output_time, retiring);
        if retiring {
            self.live = false;
            self.handover.retired.store(self.generation, Ordering::Release);
        }
    }

    fn render_live<I: Buffers + ?Sized, O: BuffersMut + ?Sized>(
        &mut self,
        input: &I,
        output: &mut O,
        input_time: Option<f64>,
        output_time: Option<f64>,
        retiring: bool,
    ) {
        let Some(out_l) = locate(output, 0, usize::MAX, self.out_left) else {
            return;
        };
        let out_r = locate(output, 0, usize::MAX, self.out_right).unwrap_or(out_l);
        let frames = frames_in(output, out_l).min(frames_in(output, out_r));

        if let Some(t) = output_time {
            if self.next_sample_time.is_some_and(|expected| (t - expected).abs() >= 1.0) {
                self.session.dropouts.fetch_add(1, Ordering::Relaxed);
            }
            self.next_sample_time = Some(t + frames as f64);
            if let Some(it) = input_time
                && t >= it
            {
                self.session.latency_frames.store((t - it).min(f64::from(u32::MAX)) as u32, Ordering::Relaxed);
            }
        }

        let mut layout_error = false;
        for lane in self.lanes.iter_mut() {
            lane.slots = lane_slots(input, &lane.map);
            layout_error |= lane.slots.is_none();
            lane.peak = 0.0;
            lane.skipped = false;
        }
        if layout_error {
            self.engine.layout_errors.fetch_add(1, Ordering::Relaxed);
        }
        let sidechain = self.sidechain_peak(frames);

        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_BLOCK_FRAMES);
            let (Some(inp), Some(out), Some(mix), Some(fin)) = (
                self.input.get_mut(..n * 2),
                self.output.get_mut(..n * 2),
                self.mix.get_mut(..n * 2),
                self.out.get_mut(..n * 2),
            ) else {
                return;
            };
            mix.fill(0.0);
            for lane in self.lanes.iter_mut() {
                let Some(mut dsp) = lane.dsp.take() else {
                    lane.skipped = true;
                    continue;
                };
                match lane.slots {
                    Some((l, r)) => {
                        gather(input, l, done, inp, 0);
                        gather(input, r, done, inp, 1);
                    }
                    None => inp.fill(0.0),
                }
                let p = peak(inp);
                lane.peak = lane.peak.max(p);
                lane.silent = if p == 0.0 { lane.silent.saturating_add(n as u64) } else { 0 };
                dsp.process(inp, out, sidechain);
                drop(dsp);
                for (m, o) in mix.iter_mut().zip(out.iter()) {
                    *m += *o;
                }
            }
            match self.master.take() {
                Some(mut master) => master.process(mix, fin, sidechain),
                // Only while a stuck old engine still holds it: a plain safety clamp.
                None => {
                    for (f, m) in fin.iter_mut().zip(mix.iter()) {
                        *f = if m.is_finite() { m.clamp(-1.0, 1.0) } else { 0.0 };
                    }
                }
            }
            apply_fades(fin, &mut self.faded_in, self.fade_len, done, frames, retiring);
            scatter(output, out_l, out_r, done, fin);
            done += n;
        }
        for lane in self.lanes.iter() {
            lane.meters.capture.raise(lane.peak);
            lane.meters.silent_frames.store(lane.silent, Ordering::Relaxed);
            if lane.skipped {
                lane.meters.gaps.fetch_add(1, Ordering::Relaxed);
            }
        }
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

/// Fades in over [`FADE_SECONDS`] after an engine goes live (`faded_in` of `fade_len` frames
/// done so far), and out over the whole of its last cycle before it lets go. `start` is the
/// block's first frame within the cycle.
fn apply_fades(
    block: &mut [f32],
    faded_in: &mut usize,
    fade_len: usize,
    start: usize,
    cycle_frames: usize,
    retiring: bool,
) {
    if *faded_in >= fade_len && !retiring {
        return;
    }
    for (f, frame) in block.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let mut gain = 1.0f32;
        if *faded_in < fade_len {
            *faded_in += 1;
            gain = *faded_in as f32 / fade_len as f32;
        }
        if retiring {
            gain *= 1.0 - (start + f + 1) as f32 / cycle_frames.max(1) as f32;
        }
        frame[0] *= gain;
        frame[1] *= gain;
    }
}

/// The slots of a lane's left and right in this cycle's input list, if the list still has the
/// layout the lane was built for.
fn lane_slots<B: Buffers + ?Sized>(input: &B, map: &LaneMap) -> Option<(Slot, Slot)> {
    let first = input.count().checked_sub(map.from_end)?;
    let end = first.checked_add(map.channels.len())?;
    if end > input.count() {
        return None;
    }
    for (k, &channels) in map.channels.iter().enumerate() {
        if input.channels(first + k) != channels {
            return None;
        }
    }
    Some((locate(input, first, end, map.left)?, locate(input, first, end, map.right)?))
}

/// Copies one side (0 = left, 1 = right) of the app's stereo into interleaved `dst`. A disabled
/// stream reads as silence; NaN and infinities from a misbehaving app read as 0.
fn gather<B: Buffers + ?Sized>(list: &B, slot: Slot, start: usize, dst: &mut [f32], side: usize) {
    let data = list.samples(slot.buffer);
    for (f, frame) in dst.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let v = data.and_then(|d| d.get((start + f) * slot.stride + slot.offset)).copied().unwrap_or(0.0);
        if let Some(x) = frame.get_mut(side) {
            *x = if v.is_finite() { v } else { 0.0 };
        }
    }
}

/// Writes interleaved stereo `src` to the output pair (folded to mono when both are one channel).
fn scatter<O: BuffersMut + ?Sized>(list: &mut O, l: Slot, r: Slot, start: usize, src: &[f32]) {
    let finite = |v: f32| if v.is_finite() { v } else { 0.0 };
    if l == r {
        let Some(data) = list.samples_mut(l.buffer) else {
            return;
        };
        for (f, [a, b]) in src.as_chunks::<2>().0.iter().enumerate() {
            if let Some(x) = data.get_mut((start + f) * l.stride + l.offset) {
                *x = finite(0.5 * (a + b));
            }
        }
    } else {
        for (slot, side) in [(l, 0), (r, 1)] {
            let Some(data) = list.samples_mut(slot.buffer) else {
                continue;
            };
            for (f, frame) in src.as_chunks::<2>().0.iter().enumerate() {
                if let (Some(x), Some(&v)) = (data.get_mut((start + f) * slot.stride + slot.offset), frame.get(side)) {
                    *x = finite(v);
                }
            }
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

    /// Multiplies its input by `gain` and records the largest block and the sidechain.
    struct Gain {
        gain: f32,
        max_block: Arc<AtomicUsize>,
        sidechain: Arc<AtomicLevel>,
        calls: Arc<AtomicUsize>,
    }

    impl Processor for Gain {
        fn process(&mut self, input: &[f32], output: &mut [f32], sidechain_peak: f32) {
            assert_eq!(input.len(), output.len());
            assert!(input.len() <= MAX_BLOCK_FRAMES * 2);
            self.max_block.fetch_max(input.len() / 2, Ordering::Relaxed);
            self.sidechain.store(sidechain_peak);
            self.calls.fetch_add(1, Ordering::Relaxed);
            for (o, i) in output.iter_mut().zip(input) {
                *o = i * self.gain;
            }
        }
    }

    #[derive(Default, Clone)]
    struct Probe {
        max_block: Arc<AtomicUsize>,
        sidechain: Arc<AtomicLevel>,
        calls: Arc<AtomicUsize>,
    }

    impl Probe {
        fn cell(&self, gain: f32) -> Arc<DspCell> {
            let p = Gain {
                gain,
                max_block: self.max_block.clone(),
                sidechain: self.sidechain.clone(),
                calls: self.calls.clone(),
            };
            Arc::new(DspCell::new(Box::new(p), 48_000))
        }
    }

    fn stereo_lane(from_end: usize, dsp: Arc<DspCell>, meters: Arc<SourceMeters>) -> LaneSetup {
        LaneSetup { map: LaneMap { from_end, channels: Box::new([2]), left: 0, right: 1 }, dsp, meters }
    }

    struct Rig {
        core: RenderCore,
        handover: Arc<Handover>,
        session: Arc<SessionMeters>,
        engine: Arc<EngineMeters>,
    }

    fn rig(generation: u32, lanes: Vec<LaneSetup>, master: Arc<DspCell>, handover: Arc<Handover>) -> Rig {
        rig_with(generation, lanes, master, handover, None, (0, 1))
    }

    fn rig_with(
        generation: u32,
        lanes: Vec<LaneSetup>,
        master: Arc<DspCell>,
        handover: Arc<Handover>,
        sidechain: Option<Arc<SidechainLevel>>,
        out: (usize, usize),
    ) -> Rig {
        let session = Arc::new(SessionMeters::default());
        let engine = Arc::new(EngineMeters::default());
        let core = RenderCore::new(CoreSetup {
            generation,
            handover: handover.clone(),
            lanes,
            master,
            out_left: out.0,
            out_right: out.1,
            sample_rate: 48_000,
            session: session.clone(),
            engine: engine.clone(),
            sidechain,
        });
        Rig { core, handover, session, engine }
    }

    /// A live single-source rig with unity master: the old one-app shape.
    fn simple(map: LaneMap, out: (usize, usize)) -> (Rig, Arc<SourceMeters>, Probe) {
        let probe = Probe::default();
        let meters = Arc::new(SourceMeters::default());
        let handover = Arc::new(Handover::default());
        let lane = LaneSetup { map, dsp: probe.cell(1.0), meters: meters.clone() };
        let mut r = rig_with(1, vec![lane], Probe::default().cell(1.0), handover, None, out);
        r.handover.activate(1);
        // Finish the fade-in so the tests below compare exact samples.
        r.core.faded_in = r.core.fade_len;
        r.core.live = true;
        (r, meters, probe)
    }

    fn ramp(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames * channels).map(|i| (i + 1) as f32 / 10_000.0).collect()
    }

    fn stereo() -> LaneMap {
        LaneMap { from_end: 1, channels: Box::new([2]), left: 0, right: 1 }
    }

    #[test]
    fn interleaved_stereo_passes_through() {
        let (mut r, meters, _) = simple(stereo(), (0, 1));
        let input = Bufs(vec![(2, Some(ramp(256, 2)))]);
        let mut output = Bufs(vec![(2, Some(vec![0.0; 512]))]);
        r.core.render(&input, &mut output, None, None);
        assert_eq!(output.0[0].1.as_deref(), input.0[0].1.as_deref());
        assert!((meters.capture.take() - 0.0512).abs() < 1e-6);
        assert_eq!(meters.silent_frames.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn several_taps_are_rendered_separately_and_summed() {
        // Input: the headset microphone (disabled), then tap A (stereo), then tap B (stereo).
        let frames = 64;
        let a: Vec<f32> = (0..frames).flat_map(|_| [0.1, 0.2]).collect();
        let b: Vec<f32> = (0..frames).flat_map(|_| [0.01, 0.02]).collect();
        let input = Bufs(vec![(1, None), (2, Some(a)), (2, Some(b))]);
        let probe = Probe::default();
        let (ma, mb) = (Arc::new(SourceMeters::default()), Arc::new(SourceMeters::default()));
        // A plays at half level, B at 10x: the sum tells which tap each lane read.
        let lanes = vec![stereo_lane(2, probe.cell(0.5), ma.clone()), stereo_lane(1, probe.cell(10.0), mb.clone())];
        let mut r = rig(1, lanes, Probe::default().cell(1.0), Arc::new(Handover::default()));
        r.handover.activate(1);
        r.core.live = true;
        r.core.faded_in = r.core.fade_len;
        let mut output = Bufs(vec![(2, Some(vec![0.0; frames * 2]))]);
        r.core.render(&input, &mut output, None, None);
        let out = output.0[0].1.as_deref().unwrap();
        for frame in out.as_chunks::<2>().0 {
            assert!((frame[0] - (0.05 + 0.1)).abs() < 1e-6, "left {}", frame[0]);
            assert!((frame[1] - (0.1 + 0.2)).abs() < 1e-6, "right {}", frame[1]);
        }
        assert!((ma.capture.take() - 0.2).abs() < 1e-6);
        assert!((mb.capture.take() - 0.02).abs() < 1e-6);
        assert_eq!(probe.calls.load(Ordering::Relaxed), 2, "each lane's processor once per block");
    }

    #[test]
    fn taps_with_different_layouts() {
        // An 8-channel device-scoped tap (the app on channels 3/4), then a stereo mixdown tap
        // split by the HAL into two mono buffers.
        let frames = 32;
        let mut wide = vec![0.0; frames * 8];
        for f in 0..frames {
            wide[f * 8 + 2] = 0.25;
            wide[f * 8 + 3] = -0.5;
        }
        let input = Bufs(vec![(8, Some(wide)), (1, Some(vec![0.125; frames])), (1, Some(vec![0.0625; frames]))]);
        let probe = Probe::default();
        let wide_map = LaneMap { from_end: 3, channels: Box::new([8]), left: 2, right: 3 };
        let split_map = LaneMap { from_end: 2, channels: Box::new([1, 1]), left: 0, right: 1 };
        let lanes = vec![
            LaneSetup { map: wide_map, dsp: probe.cell(1.0), meters: Arc::default() },
            LaneSetup { map: split_map, dsp: probe.cell(1.0), meters: Arc::default() },
        ];
        // Output: an 8-channel interface whose stereo pair is 3/4.
        let mut r = rig_with(1, lanes, Probe::default().cell(1.0), Arc::default(), None, (2, 3));
        r.handover.activate(1);
        r.core.live = true;
        r.core.faded_in = r.core.fade_len;
        let mut output = Bufs(vec![(8, Some(vec![0.0; frames * 8]))]);
        r.core.render(&input, &mut output, None, None);
        let out = output.0[0].1.as_deref().unwrap();
        for f in 0..frames {
            assert_eq!(&out[f * 8..f * 8 + 8], &[0.0, 0.0, 0.375, -0.4375, 0.0, 0.0, 0.0, 0.0]);
        }
    }

    #[test]
    fn non_interleaved_output_and_mono_fold() {
        let tap = ramp(64, 2);
        let input = Bufs(vec![(2, Some(tap.clone()))]);
        let (mut r, _, _) = simple(stereo(), (2, 3));
        let mut output = Bufs((0..4).map(|_| (1, Some(vec![0.0; 64]))).collect());
        r.core.render(&input, &mut output, None, None);
        let left: Vec<f32> = tap.iter().step_by(2).copied().collect();
        let right: Vec<f32> = tap.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(output.0[2].1.as_deref(), Some(&left[..]));
        assert_eq!(output.0[3].1.as_deref(), Some(&right[..]));
        assert!(output.0[0].1.as_ref().is_some_and(|b| b.iter().all(|&s| s == 0.0)));

        let (mut r, _, _) = simple(LaneMap { from_end: 1, channels: Box::new([1]), left: 0, right: 0 }, (0, 0));
        let input = Bufs(vec![(1, Some(vec![0.5; 16]))]);
        let mut output = Bufs(vec![(1, Some(vec![0.0; 16]))]);
        r.core.render(&input, &mut output, None, None);
        assert!(output.0[0].1.as_ref().unwrap().iter().all(|&s| s == 0.5));
    }

    #[test]
    fn taps_are_found_from_the_end_of_the_input_list() {
        let tap = ramp(32, 2);
        // A live headset microphone (its streams could not be turned off) must not be read.
        let mic = vec![0.9; 32];
        let with_mic = Bufs(vec![(1, Some(mic.clone())), (1, Some(mic)), (2, Some(tap.clone()))]);
        // The same cycle if the HAL left the disabled microphone streams out of the list.
        let without_mic = Bufs(vec![(2, Some(tap.clone()))]);
        for input in [with_mic, without_mic] {
            let (mut r, _, _) = simple(stereo(), (0, 1));
            let mut output = Bufs(vec![(2, Some(vec![0.0; 64]))]);
            r.core.render(&input, &mut output, None, None);
            assert_eq!(output.0[0].1.as_deref(), Some(&tap[..]));
        }
    }

    #[test]
    fn a_changed_layout_reads_as_silence_and_is_counted() {
        let (mut r, meters, _) = simple(stereo(), (0, 1));
        // The tap now arrives with 4 channels: not what the lane was built for.
        let input = Bufs(vec![(4, Some(vec![0.3; 64]))]);
        let mut output = Bufs(vec![(2, Some(vec![1.0; 32]))]);
        r.core.render(&input, &mut output, None, None);
        assert!(output.0[0].1.as_ref().unwrap().iter().all(|&s| s == 0.0));
        assert_eq!(r.engine.layout_errors.load(Ordering::Relaxed), 1);
        assert_eq!(meters.capture.take(), 0.0);
        // Fewer buffers than the lane needs.
        let mut r2 = simple(LaneMap { from_end: 3, channels: Box::new([2]), left: 0, right: 1 }, (0, 1)).0;
        r2.core.render(&Bufs(vec![(2, Some(vec![0.3; 64]))]), &mut output, None, None);
        assert_eq!(r2.engine.layout_errors.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn large_buffers_are_chunked() {
        let frames = MAX_BLOCK_FRAMES * 2 + 100;
        let input = Bufs(vec![(2, Some(ramp(frames, 2)))]);
        let mut output = Bufs(vec![(2, Some(vec![0.0; frames * 2]))]);
        let (mut r, _, probe) = simple(stereo(), (0, 1));
        r.core.render(&input, &mut output, None, None);
        assert_eq!(probe.max_block.load(Ordering::Relaxed), MAX_BLOCK_FRAMES);
        assert_eq!(output.0[0].1.as_deref(), input.0[0].1.as_deref(), "chunks line up seamlessly");
    }

    #[test]
    fn silence_garbage_and_short_input() {
        let (mut r, meters, _) = simple(stereo(), (0, 1));
        let disabled = Bufs(vec![(2, None)]);
        let mut output = Bufs(vec![(2, Some(vec![1.0; 256]))]);
        r.core.render(&disabled, &mut output, None, None);
        r.core.render(&disabled, &mut output, None, None);
        assert!(output.0[0].1.as_ref().unwrap().iter().all(|&s| s == 0.0));
        assert_eq!(meters.silent_frames.load(Ordering::Relaxed), 256);
        let loud = Bufs(vec![(2, Some(vec![0.1; 256]))]);
        r.core.render(&loud, &mut output, None, None);
        assert_eq!(meters.silent_frames.load(Ordering::Relaxed), 0);

        let garbage = Bufs(vec![(2, Some(vec![f32::NAN, f32::INFINITY, 0.25, 0.25]))]);
        let mut output = Bufs(vec![(2, Some(vec![9.0; 8]))]);
        r.core.render(&garbage, &mut output, None, None);
        assert_eq!(output.0[0].1.as_deref(), Some(&[0.0, 0.0, 0.25, 0.25, 0.0, 0.0, 0.0, 0.0][..]));
    }

    #[test]
    fn missing_output_pair_is_a_no_op() {
        let (mut r, _, probe) = simple(stereo(), (0, 1));
        let input = Bufs(vec![(2, Some(vec![0.5; 8]))]);
        let mut output = Bufs(vec![]);
        r.core.render(&input, &mut output, None, None);
        assert_eq!(r.engine.cycles.load(Ordering::Relaxed), 1);
        assert_eq!(probe.calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn timestamp_gaps_count_as_dropouts() {
        let (mut r, _, _) = simple(stereo(), (0, 1));
        let input = Bufs(vec![(2, Some(vec![0.0; 1024]))]);
        let mut output = Bufs(vec![(2, Some(vec![0.0; 1024]))]);
        r.core.render(&input, &mut output, Some(0.0), Some(1000.0));
        r.core.render(&input, &mut output, Some(512.0), Some(1512.0));
        assert_eq!(r.session.dropouts.load(Ordering::Relaxed), 0);
        r.core.render(&input, &mut output, Some(4096.0), Some(5096.0));
        assert_eq!(r.session.dropouts.load(Ordering::Relaxed), 1);
        assert_eq!(r.session.latency_frames.load(Ordering::Relaxed), 1000);
    }

    #[test]
    fn stale_sidechain_stops_ducking() {
        let sc = Arc::new(SidechainLevel::default());
        let probe = Probe::default();
        let lane = stereo_lane(1, probe.cell(1.0), Arc::default());
        let mut r = rig_with(1, vec![lane], Probe::default().cell(1.0), Arc::default(), Some(sc.clone()), (0, 1));
        r.handover.activate(1);
        let input = Bufs(vec![(2, Some(vec![0.0; 1024]))]);
        let mut output = Bufs(vec![(2, Some(vec![0.0; 1024]))]);
        r.core.render(&input, &mut output, None, None);
        assert_eq!(probe.sidechain.load(), 0.0, "a level from before the core is not fresh");
        meter_sidechain(&sc, &Bufs(vec![(2, Some(vec![0.0, -0.8, 0.3, 0.1]))]));
        assert_eq!(sc.meter.take(), 0.8);
        r.core.render(&input, &mut output, None, None);
        assert_eq!(probe.sidechain.load(), 0.8);
        // 12000 frames (250 ms at 48 kHz) of no sidechain updates: treated as gone.
        for _ in 0..30 {
            r.core.render(&input, &mut output, None, None);
        }
        assert_eq!(probe.sidechain.load(), 0.0);
        meter_sidechain(&sc, &Bufs(vec![(2, Some(vec![0.4, 0.0]))]));
        r.core.render(&input, &mut output, None, None);
        assert_eq!(probe.sidechain.load(), 0.4);
    }

    #[test]
    fn locate_counts_channels_across_buffers() {
        let list = Bufs(vec![(2, None), (1, None), (4, None)]);
        assert_eq!(locate(&list, 0, 3, 0), Some(Slot { buffer: 0, offset: 0, stride: 2 }));
        assert_eq!(locate(&list, 0, 3, 2), Some(Slot { buffer: 1, offset: 0, stride: 1 }));
        assert_eq!(locate(&list, 0, 3, 5), Some(Slot { buffer: 2, offset: 2, stride: 4 }));
        assert_eq!(locate(&list, 1, 3, 1), Some(Slot { buffer: 2, offset: 0, stride: 4 }));
        assert_eq!(locate(&list, 0, 2, 3), None, "beyond the lane's last buffer");
        assert_eq!(locate(&list, 0, 3, 7), None);
        assert_eq!(locate(&list, 9, 12, 0), None);
    }

    /// Two engines over the same cells, as during a rebuild: the old one renders, fades out
    /// once it is paused and reports that it let go; the new one waits until it is activated
    /// and then fades in. The kept source's processor runs on exactly one of them per cycle.
    #[test]
    fn handover_between_engines() {
        let handover = Arc::new(Handover::default());
        let probe = Probe::default();
        let kept = probe.cell(1.0);
        let master = Probe::default().cell(1.0);
        let meters = Arc::new(SourceMeters::default());
        let mut old = rig(1, vec![stereo_lane(1, kept.clone(), meters.clone())], master.clone(), handover.clone());
        let mut new = rig(2, vec![stereo_lane(1, kept.clone(), meters.clone())], master.clone(), handover.clone());
        let input = Bufs(vec![(2, Some(vec![0.5; 2048]))]);
        let out = |b: &Bufs| b.0[0].1.clone().unwrap();
        let mut o1 = Bufs(vec![(2, Some(vec![0.0; 2048]))]);
        let mut o2 = Bufs(vec![(2, Some(vec![0.0; 2048]))]);

        handover.activate(1);
        old.core.render(&input, &mut o1, None, None);
        new.core.render(&input, &mut o2, None, None);
        assert!(out(&o2).iter().all(|&s| s == 0.0), "standby engine stays silent");
        let first = out(&o1);
        assert_eq!(first[0], 0.5 / old.core.fade_len as f32, "fades in");
        assert_eq!(first[2047], 0.5);
        assert_eq!(probe.calls.load(Ordering::Relaxed), 1);

        handover.pause();
        old.core.render(&input, &mut o1, None, None);
        let last = out(&o1);
        assert!(last[0] > 0.49 && last[2046] < 0.001 && last[2047] == 0.0, "fades out over its last cycle");
        assert!(handover.has_retired(1));
        new.core.render(&input, &mut o2, None, None);
        assert!(out(&o2).iter().all(|&s| s == 0.0), "nobody renders while paused");

        handover.activate(2);
        o1.0[0].1 = Some(vec![0.0; 2048]);
        old.core.render(&input, &mut o1, None, None);
        new.core.render(&input, &mut o2, None, None);
        assert!(out(&o1).iter().all(|&s| s == 0.0), "the retired engine stays silent");
        assert_eq!(out(&o2)[2047], 0.5);
        assert_eq!(probe.calls.load(Ordering::Relaxed), 3);
        assert_eq!(meters.gaps.load(Ordering::Relaxed), 0);
    }

    /// Counts the frames it has processed: its state shows whether the same processor carried
    /// on.
    struct Count(Arc<AtomicUsize>);

    impl Processor for Count {
        fn process(&mut self, _: &[f32], output: &mut [f32], _: f32) {
            let frames = output.len() / 2;
            self.0.fetch_add(frames, Ordering::Relaxed);
            output.fill(0.0);
        }
    }

    /// A source change as the session does it: sources A and B play, then B and C. B is the
    /// same processor in both engines and carries on; A stops with the old engine and C starts
    /// with the new one, and nothing runs twice in a cycle.
    #[test]
    fn a_kept_source_carries_on_across_engines() {
        let handover = Arc::new(Handover::default());
        let (a, c) = (Probe::default(), Probe::default());
        let counted = Arc::new(AtomicUsize::new(0));
        let b = Arc::new(DspCell::new(Box::new(Count(counted.clone())), 48_000));
        let master = Probe::default().cell(1.0);
        let old_lanes = vec![stereo_lane(2, a.cell(1.0), Arc::default()), stereo_lane(1, b.clone(), Arc::default())];
        // In the new aggregate B's tap comes first.
        let new_lanes = vec![stereo_lane(2, b.clone(), Arc::default()), stereo_lane(1, c.cell(1.0), Arc::default())];
        let mut old = rig(1, old_lanes, master.clone(), handover.clone());
        let mut new = rig(2, new_lanes, master.clone(), handover.clone());
        let input = Bufs(vec![(2, Some(vec![0.1; 128])), (2, Some(vec![0.2; 128]))]);
        let mut out = Bufs(vec![(2, Some(vec![0.0; 128]))]);

        handover.activate(1);
        old.core.render(&input, &mut out, None, None);
        new.core.render(&input, &mut out, None, None);
        assert_eq!(counted.load(Ordering::Relaxed), 64);
        assert_eq!((a.calls.load(Ordering::Relaxed), c.calls.load(Ordering::Relaxed)), (1, 0));

        handover.pause();
        old.core.render(&input, &mut out, None, None);
        new.core.render(&input, &mut out, None, None);
        assert!(handover.has_retired(1));
        assert_eq!(counted.load(Ordering::Relaxed), 128, "the old engine's last cycle");

        handover.activate(2);
        old.core.render(&input, &mut out, None, None);
        new.core.render(&input, &mut out, None, None);
        assert_eq!(counted.load(Ordering::Relaxed), 192, "B's processor carried on in the new engine");
        assert_eq!(a.calls.load(Ordering::Relaxed), 2, "A ended with the old engine");
        assert_eq!(c.calls.load(Ordering::Relaxed), 1, "C started with the new engine");
        assert_eq!(new.engine.cycles.load(Ordering::Relaxed), 3, "standby cycles count too");
    }

    #[test]
    fn a_processor_held_elsewhere_is_skipped_not_shared() {
        let probe = Probe::default();
        let cell = probe.cell(1.0);
        let meters = Arc::new(SourceMeters::default());
        let (mut r, _, _) = simple(stereo(), (0, 1));
        r.core.lanes = vec![Lane {
            map: stereo(),
            dsp: cell.clone(),
            meters: meters.clone(),
            slots: None,
            silent: 0,
            peak: 0.0,
            skipped: false,
        }]
        .into_boxed_slice();
        let held = cell.take().expect("free");
        let input = Bufs(vec![(2, Some(vec![0.5; 64]))]);
        let mut output = Bufs(vec![(2, Some(vec![0.0; 64]))]);
        r.core.render(&input, &mut output, None, None);
        assert_eq!(probe.calls.load(Ordering::Relaxed), 0);
        assert_eq!(meters.gaps.load(Ordering::Relaxed), 1);
        assert!(output.0[0].1.as_ref().unwrap().iter().all(|&s| s == 0.0));
        drop(held);
        r.core.render(&input, &mut output, None, None);
        assert_eq!(probe.calls.load(Ordering::Relaxed), 1);
        assert!(cell.take().is_some(), "put back after the cycle");
    }

    #[test]
    fn master_runs_on_the_sum_and_falls_back_to_a_clamp() {
        let probe = Probe::default();
        let lanes =
            vec![stereo_lane(2, probe.cell(1.0), Arc::default()), stereo_lane(1, probe.cell(1.0), Arc::default())];
        let master = Probe::default().cell(0.5);
        let mut r = rig(1, lanes, master.clone(), Arc::default());
        r.handover.activate(1);
        r.core.live = true;
        r.core.faded_in = r.core.fade_len;
        let input = Bufs(vec![(2, Some(vec![0.8; 32])), (2, Some(vec![0.8; 32]))]);
        let mut output = Bufs(vec![(2, Some(vec![0.0; 32]))]);
        r.core.render(&input, &mut output, None, None);
        assert!(output.0[0].1.as_ref().unwrap().iter().all(|&s| (s - 0.8).abs() < 1e-6), "master halves 1.6");
        let held = master.take().expect("free");
        r.core.render(&input, &mut output, None, None);
        assert!(output.0[0].1.as_ref().unwrap().iter().all(|&s| s == 1.0), "clamped sum without the master");
        drop(held);
    }

    #[test]
    fn generations_skip_idle() {
        assert_eq!(next_generation(IDLE), 1);
        assert_eq!(next_generation(41), 42);
        assert_eq!(next_generation(u32::MAX), 1);
    }

    #[test]
    fn cells_free_their_processor() {
        struct Flag(Arc<AtomicUsize>);
        impl Processor for Flag {
            fn process(&mut self, _: &[f32], output: &mut [f32], _: f32) {
                output.fill(0.0);
            }
        }
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let cell = DspCell::new(Box::new(Flag(dropped.clone())), 44_100);
        assert_eq!(cell.rate(), 44_100);
        drop(cell);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }
}
