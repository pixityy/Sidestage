//! The mix the output stream plays: every source's bridge, through that source's processor,
//! summed, through the master processor.
//!
//! The [`Mixer`] lives on the real-time output thread; the session thread changes its sources
//! through a [`MixerControl`] while it plays. Both directions are lock-free SPSC rings: sources
//! (with the processor the factory built on the session thread) go in, and removed sources come
//! back out, so their memory is freed on the session thread rather than the real-time one. The
//! slot list is allocated for [`MAX_SOURCES`] up front, so adding a source never reallocates it.

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::bridge::BridgeConsumer;
use crate::{AtomicLevel, MAX_BLOCK_FRAMES, Processor, SourceId, peak};

/// Most sources one session plays. Each one is a null sink, a capture connection and thread and
/// a mixer slot; the limit only bounds the memory reserved for the slots.
pub const MAX_SOURCES: usize = 32;
/// Commands and returned slots in flight. Twice the slots, so replacing every source at once
/// (a remove and an add each) fits, plus slack.
const QUEUE: usize = 2 * MAX_SOURCES + 8;

/// One source as the output thread sees it.
pub struct SourceSlot {
    id: SourceId,
    consumer: BridgeConsumer,
    processor: Box<dyn Processor>,
    /// Peak of the captured audio, for the source's status.
    capture_peak: Arc<AtomicLevel>,
    /// Set when the processor panicked; the source's capture thread reports it so the session
    /// rebuilds the source.
    crashed: Arc<AtomicBool>,
    /// The processor panicked: it is never called again.
    faulted: bool,
}

impl SourceSlot {
    pub fn new(
        id: SourceId,
        consumer: BridgeConsumer,
        processor: Box<dyn Processor>,
        capture_peak: Arc<AtomicLevel>,
        crashed: Arc<AtomicBool>,
    ) -> Box<Self> {
        Box::new(Self { id, consumer, processor, capture_peak, crashed, faulted: false })
    }

    #[cfg(test)]
    pub fn id(&self) -> SourceId {
        self.id
    }
}

enum Command {
    Add(Box<SourceSlot>),
    Remove(SourceId),
}

/// The session thread's end.
pub struct MixerControl {
    commands: rtrb::Producer<Command>,
    returned: rtrb::Consumer<Box<SourceSlot>>,
}

impl MixerControl {
    /// Queues a source; it plays from the next block. Gives the slot back if the queue is full
    /// (the output thread is not taking commands).
    pub fn add(&mut self, slot: Box<SourceSlot>) -> Result<(), Box<SourceSlot>> {
        if self.commands.is_full() {
            return Err(slot);
        }
        // Cannot fail: there is room and this is the only producer.
        let _ = self.commands.push(Command::Add(slot));
        Ok(())
    }

    /// Queues the removal of source `id`. False if the queue is full.
    pub fn remove(&mut self, id: SourceId) -> bool {
        self.commands.push(Command::Remove(id)).is_ok()
    }

    /// Frees the slots the output thread has handed back. Call regularly: the output thread
    /// only removes a source while there is room to hand it back.
    pub fn collect(&mut self) {
        while let Ok(slot) = self.returned.pop() {
            drop(slot);
        }
    }
}

/// The output thread's end.
pub struct Mixer {
    /// Boxed so a slot travels through the rings and in and out of this list as one pointer.
    #[allow(clippy::vec_box)]
    slots: Vec<Box<SourceSlot>>,
    commands: rtrb::Consumer<Command>,
    returned: rtrb::Producer<Box<SourceSlot>>,
    master: Box<dyn Processor>,
    input: Vec<f32>,
    output: Vec<f32>,
    mix: Vec<f32>,
}

/// A mixer with no sources whose sum runs through `master`.
pub fn mixer(master: Box<dyn Processor>) -> (MixerControl, Mixer) {
    let (commands_tx, commands_rx) = rtrb::RingBuffer::new(QUEUE);
    let (returned_tx, returned_rx) = rtrb::RingBuffer::new(QUEUE);
    let block = vec![0.0; MAX_BLOCK_FRAMES * 2];
    (
        MixerControl { commands: commands_tx, returned: returned_rx },
        Mixer {
            slots: Vec::with_capacity(MAX_SOURCES),
            commands: commands_rx,
            returned: returned_tx,
            master,
            input: block.clone(),
            output: block.clone(),
            mix: block,
        },
    )
}

impl Mixer {
    /// Applies queued source changes. [`Mixer::render`] does this too; the output thread also
    /// calls it while the server is not asking for audio, so removed sources still come back.
    pub fn apply_commands(&mut self) {
        // Every command may hand a slot back: wait for room rather than free it here.
        while self.returned.slots() > 0 {
            let Ok(command) = self.commands.pop() else {
                return;
            };
            let back = match command {
                Command::Add(slot) if self.slots.len() < self.slots.capacity() => {
                    // Within capacity: no reallocation.
                    self.slots.push(slot);
                    None
                }
                // The session never sends more than MAX_SOURCES; refuse rather than reallocate.
                Command::Add(slot) => Some(slot),
                Command::Remove(id) => self.slots.iter().position(|s| s.id == id).map(|i| self.slots.swap_remove(i)),
            };
            if let Some(slot) = back {
                // Cannot fail: room was checked and this thread is the only producer.
                let _ = self.returned.push(slot);
            }
        }
    }

    /// Fills `out` (interleaved stereo, at most [`MAX_BLOCK_FRAMES`] frames) with every source's
    /// processed audio, summed and run through the master processor. Real-time safe.
    pub fn render(&mut self, out: &mut [f32], sidechain_peak: f32) {
        debug_assert!(out.len() <= MAX_BLOCK_FRAMES * 2 && out.len().is_multiple_of(2));
        self.apply_commands();
        let n = out.len().min(MAX_BLOCK_FRAMES * 2);
        let Self { slots, master, input, output, mix, .. } = self;
        let (input, output, mix) = (&mut input[..n], &mut output[..n], &mut mix[..n]);
        mix.fill(0.0);
        for slot in slots.iter_mut() {
            // Keep reading a faulted source so its bridge does not count overruns.
            slot.consumer.pull(input);
            if slot.faulted {
                continue;
            }
            slot.capture_peak.raise(peak(input));
            let processor = &mut slot.processor;
            let input = &*input;
            let result =
                panic::catch_unwind(AssertUnwindSafe(|| processor.process(input, &mut *output, sidechain_peak)));
            if result.is_err() {
                // One source's renderer failing must not silence the others (contract: problems
                // with one source never affect the others). Its state may be broken: never call
                // it again; the session rebuilds the source.
                slot.faulted = true;
                slot.crashed.store(true, Ordering::Relaxed);
                continue;
            }
            for (m, o) in mix.iter_mut().zip(output.iter()) {
                *m += *o;
            }
        }
        master.process(mix, &mut out[..n], sidechain_peak);
    }

    #[cfg(test)]
    pub fn sources(&self) -> usize {
        self.slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{BridgeConfig, bridge};

    /// Multiplies by a gain and remembers the last sidechain level it saw.
    struct Gain(f32);
    impl Processor for Gain {
        fn process(&mut self, input: &[f32], output: &mut [f32], _sidechain_peak: f32) {
            for (o, i) in output.iter_mut().zip(input) {
                *o = i * self.0;
            }
        }
    }

    /// Writes the sidechain level, to check what every processor is given.
    struct Sidechain;
    impl Processor for Sidechain {
        fn process(&mut self, _input: &[f32], output: &mut [f32], sidechain_peak: f32) {
            output.fill(sidechain_peak);
        }
    }

    struct Panics;
    impl Processor for Panics {
        fn process(&mut self, _input: &[f32], _output: &mut [f32], _sidechain_peak: f32) {
            panic!("test processor panics on purpose");
        }
    }

    struct Source {
        peak: Arc<AtomicLevel>,
        crashed: Arc<AtomicBool>,
    }

    impl Source {
        /// The mixer let go of this source's slot (it holds a clone of `peak`).
        fn freed(&self) -> bool {
            Arc::strong_count(&self.peak) == 1
        }
    }

    /// A source fed with a constant `level`, primed (1 ms bridge target) and past the
    /// interpolator's ramp-in, so every block it plays is exactly `level`.
    fn source(id: SourceId, processor: Box<dyn Processor>, level: f32) -> (Box<SourceSlot>, Source) {
        let (mut producer, mut consumer, _) = bridge(BridgeConfig::new(48_000, 1.0));
        producer.push(&vec![level; 4096 * 2]);
        consumer.pull(&mut [0.0; 8 * 2]);
        let peak = Arc::new(AtomicLevel::new());
        let crashed = Arc::new(AtomicBool::new(false));
        let slot = SourceSlot::new(id, consumer, processor, peak.clone(), crashed.clone());
        (slot, Source { peak, crashed })
    }

    fn render(m: &mut Mixer, sidechain: f32) -> Vec<f32> {
        let mut out = vec![9.0; 64];
        m.render(&mut out, sidechain);
        out
    }

    #[test]
    fn sums_sources_through_their_processors_and_the_master() {
        let (mut control, mut m) = mixer(Box::new(Gain(0.5)));
        assert!(render(&mut m, 0.0).iter().all(|&s| s == 0.0), "no sources: silence, never nothing");

        let (a, sa) = source(1, Box::new(Gain(2.0)), 0.25);
        let (b, sb) = source(2, Box::new(Gain(1.0)), 0.125);
        assert!(control.add(a).is_ok() && control.add(b).is_ok());
        let out = render(&mut m, 0.0);
        assert_eq!(m.sources(), 2);
        assert!(out.iter().all(|&s| (s - 0.5 * (0.5 + 0.125)).abs() < 1e-6), "{out:?}");
        assert_eq!(sa.peak.take(), 0.25, "capture peak is the processor's input");
        assert_eq!(sb.peak.take(), 0.125);

        assert!(control.remove(1));
        let out = render(&mut m, 0.0);
        assert!(out.iter().all(|&s| (s - 0.5 * 0.125).abs() < 1e-6), "removed source no longer plays");
        assert_eq!(m.sources(), 1);
        control.collect();
        assert!(sa.freed(), "the removed slot came back and was freed off the output thread");
        assert!(!sb.freed());
        assert!(control.remove(42), "removing an unknown source is harmless");
        render(&mut m, 0.0);
        assert_eq!(m.sources(), 1);
    }

    #[test]
    fn every_processor_gets_the_sidechain_level() {
        let (mut control, mut m) = mixer(Box::new(Gain(1.0)));
        let (a, _sa) = source(1, Box::new(Sidechain), 0.0);
        let (b, _sb) = source(2, Box::new(Sidechain), 0.0);
        assert!(control.add(a).is_ok() && control.add(b).is_ok());
        assert!(render(&mut m, 0.25).iter().all(|&s| (s - 0.5).abs() < 1e-6));
        let (_, mut m) = mixer(Box::new(Sidechain));
        assert!(render(&mut m, 0.75).iter().all(|&s| s == 0.75), "and the master");
    }

    #[test]
    fn a_panicking_source_is_muted_without_affecting_the_others() {
        let (mut control, mut m) = mixer(Box::new(Gain(1.0)));
        let (good, sg) = source(1, Box::new(Gain(1.0)), 0.25);
        let (bad, sbad) = source(2, Box::new(Panics), 0.5);
        assert!(control.add(good).is_ok() && control.add(bad).is_ok());
        for _ in 0..3 {
            assert!(render(&mut m, 0.0).iter().all(|&s| (s - 0.25).abs() < 1e-6));
        }
        assert!(sbad.crashed.load(Ordering::Relaxed), "reported for the session to rebuild it");
        assert!(!sg.crashed.load(Ordering::Relaxed));
        assert_eq!(sbad.peak.take(), 0.5, "measured before the crash, never after");
        render(&mut m, 0.0);
        assert_eq!(sbad.peak.take(), 0.0);
    }

    #[test]
    fn slots_are_bounded_and_never_freed_on_the_output_thread() {
        let (mut control, mut m) = mixer(Box::new(Gain(1.0)));
        let mut feeds = Vec::new();
        for id in 0..MAX_SOURCES as u32 + 1 {
            let (slot, feed) = source(id, Box::new(Gain(1.0)), 0.0);
            assert!(control.add(slot).is_ok());
            feeds.push(feed);
        }
        render(&mut m, 0.0);
        assert_eq!(m.sources(), MAX_SOURCES, "the slot list never grows past what was reserved");
        control.collect();
        assert!(feeds.last().unwrap().freed(), "the extra source was handed back");

        // With the return queue full, removals wait instead of freeing memory on this thread.
        let (mut control, mut m) = mixer(Box::new(Gain(1.0)));
        let mut kept = Vec::new();
        for id in 0..MAX_SOURCES as u32 {
            let (slot, feed) = source(id, Box::new(Gain(1.0)), 0.0);
            assert!(control.add(slot).is_ok());
            kept.push(feed);
        }
        render(&mut m, 0.0);
        let mut refused = Vec::new();
        for id in 100..100 + QUEUE as u32 {
            let (slot, feed) = source(id, Box::new(Gain(1.0)), 0.0);
            assert!(control.add(slot).is_ok());
            refused.push(feed);
        }
        render(&mut m, 0.0);
        assert!(!refused.iter().any(|f| f.freed()), "handed back, not freed here");
        assert!(control.remove(0));
        render(&mut m, 0.0);
        assert_eq!(m.sources(), MAX_SOURCES, "the removal waits while nothing can be handed back");
        control.collect();
        assert!(refused.iter().all(|f| f.freed()));
        render(&mut m, 0.0);
        assert_eq!(m.sources(), MAX_SOURCES - 1, "and goes ahead once the session collected");
        control.collect();
        assert!(kept[0].freed() && !kept[1].freed());
    }

    #[test]
    fn full_command_queue_gives_the_slot_back() {
        let (mut control, _m) = mixer(Box::new(Gain(1.0)));
        for _ in 0..QUEUE {
            assert!(control.remove(1));
        }
        assert!(!control.remove(1));
        let (slot, _feed) = source(7, Box::new(Gain(1.0)), 0.0);
        let back = control.add(slot).expect_err("queue is full");
        assert_eq!(back.id(), 7);
    }
}
