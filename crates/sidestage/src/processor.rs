//! Glue between the backends and the renderer: `sidestage-platform` knows nothing about the DSP,
//! it only calls the [`Processor`]s built by the [`ProcessorFactory`]s made here.
//!
//! Every source gets its own [`Spatializer`] (its own position and preset). Voice chat ducking
//! needs the sources to hear each other: a source whose settings say `voice_chat` publishes its
//! input level on a shared [`VoiceBus`], and every source's renderer reads the loudest *other*
//! voice-chat source from it. The bus is a fixed array of atomics, so the real-time thread
//! neither locks nor allocates. The session's master bus ([`MasterBus`]) limits the mix.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use sidestage_dsp::{MasterBus, SharedParams, Spatializer};
use sidestage_platform::{AtomicLevel, Processor, ProcessorFactory, SourceId, peak};

use crate::config::MAX_SOURCES;

/// Slots on the bus. Twice the most sources a session has, so a removed source's slot (whose
/// renderer the backend may still be running for a moment) is not handed to a new source at
/// once.
pub const VOICE_SLOTS: usize = 2 * MAX_SOURCES;
/// A slot whose writer has not published for this long counts as silent: its renderer was
/// dropped without saying so, or the backend stopped calling it.
const STALE_SECONDS: f32 = 0.25;

/// The voice-chat levels of a session's sources, one slot per source.
#[derive(Debug, Default)]
pub struct VoiceBus {
    slots: [VoiceSlot; VOICE_SLOTS],
}

#[derive(Debug, Default)]
struct VoiceSlot {
    /// Input peak of the writer's last block while it is voice chat, else 0.
    level: AtomicLevel,
    /// Bumped with every publish, so readers can tell a live writer from a stale value.
    beat: AtomicU32,
}

impl VoiceBus {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Real-time safe.
    #[inline]
    fn publish(&self, slot: usize, level: f32) {
        if let Some(s) = self.slots.get(slot) {
            s.level.store(level);
            s.beat.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn clear(&self, slot: usize) {
        if let Some(s) = self.slots.get(slot) {
            s.level.store(0.0);
        }
    }
}

/// What one reader remembers about the other slots, to ignore stale ones. Lives inside the
/// reader's processor: fixed size, no allocation.
#[derive(Debug)]
struct VoiceWatch {
    beat: [u32; VOICE_SLOTS],
    /// Frames this reader processed since the slot's writer last published.
    quiet: [u32; VOICE_SLOTS],
    stale_after: u32,
}

impl VoiceWatch {
    /// Every slot starts stale: a value left on the bus by a writer that is long gone must not
    /// count. A live writer publishes again with its next block.
    fn new(sample_rate: u32, bus: &VoiceBus) -> Self {
        let stale_after = (sample_rate as f32 * STALE_SECONDS) as u32;
        let beat = std::array::from_fn(|i| bus.slots[i].beat.load(Ordering::Relaxed));
        Self { beat, quiet: [stale_after; VOICE_SLOTS], stale_after }
    }

    /// The loudest live voice-chat level among the slots other than `own`. Real-time safe.
    #[inline]
    fn loudest_other(&mut self, bus: &VoiceBus, own: Option<usize>, frames: u32) -> f32 {
        let mut loudest = 0.0f32;
        for (i, slot) in bus.slots.iter().enumerate() {
            if Some(i) == own {
                continue;
            }
            let beat = slot.beat.load(Ordering::Relaxed);
            if beat != self.beat[i] {
                self.beat[i] = beat;
                self.quiet[i] = 0;
            } else {
                self.quiet[i] = self.quiet[i].saturating_add(frames);
            }
            if self.quiet[i] < self.stale_after {
                loudest = loudest.max(slot.level.load());
            }
        }
        loudest
    }
}

/// Hands out [`VoiceBus`] slots to sources. A freed slot goes to the back of the line, so it is
/// reused as late as possible.
#[derive(Debug)]
pub struct VoiceSlots {
    owners: [Option<SourceId>; VOICE_SLOTS],
    /// Slot indices, least recently freed first.
    free: Vec<usize>,
}

impl Default for VoiceSlots {
    fn default() -> Self {
        Self { owners: [None; VOICE_SLOTS], free: (0..VOICE_SLOTS).collect() }
    }
}

impl VoiceSlots {
    /// The source's slot, assigning one if it has none. `None` only when every slot is taken.
    pub fn assign(&mut self, id: SourceId) -> Option<usize> {
        if let Some(slot) = self.slot_of(id) {
            return Some(slot);
        }
        if self.free.is_empty() {
            return None;
        }
        let slot = self.free.remove(0);
        self.owners[slot] = Some(id);
        Some(slot)
    }

    pub fn slot_of(&self, id: SourceId) -> Option<usize> {
        self.owners.iter().position(|o| *o == Some(id))
    }

    /// Free the slots of every source not in `keep`.
    pub fn retain(&mut self, keep: impl Fn(SourceId) -> bool) {
        for (slot, owner) in self.owners.iter_mut().enumerate() {
            if owner.is_some_and(|id| !keep(id)) {
                *owner = None;
                self.free.push(slot);
            }
        }
    }
}

/// One source's binaural renderer as a backend [`Processor`].
struct SourceProcessor {
    renderer: Spatializer,
    bus: Arc<VoiceBus>,
    slot: Option<usize>,
    watch: VoiceWatch,
}

impl Processor for SourceProcessor {
    #[inline]
    fn process(&mut self, input: &[f32], output: &mut [f32], sidechain_peak: f32) {
        let frames = (input.len() / 2) as u32;
        let voice = self.watch.loudest_other(&self.bus, self.slot, frames);
        self.renderer.process_with_voice(input, output, sidechain_peak, voice);
        if let Some(slot) = self.slot {
            // The settings this block was rendered with, so a source switched to voice chat
            // starts (or stops) turning the others down with its next block.
            let level = if self.renderer.settings().voice_chat { peak(input) } else { 0.0 };
            self.bus.publish(slot, level);
        }
    }
}

impl Drop for SourceProcessor {
    fn drop(&mut self) {
        // A removed voice source stops ducking the others at once instead of after the stale
        // timeout. If the backend has already built this source's next renderer, that one
        // publishes again with its next block.
        if let Some(slot) = self.slot {
            self.bus.clear(slot);
        }
    }
}

/// A factory that builds a fresh renderer for each output stream the backend opens for this
/// source. All of them read the same `params`, so UI changes reach whichever stream is current.
pub fn source_factory(params: Arc<SharedParams>, bus: Arc<VoiceBus>, slot: Option<usize>) -> ProcessorFactory {
    Arc::new(move |sample_rate| {
        log::debug!("building a source renderer at {sample_rate} Hz (voice slot {slot:?})");
        Box::new(SourceProcessor {
            renderer: Spatializer::new(sample_rate, Arc::clone(&params)),
            bus: Arc::clone(&bus),
            slot,
            watch: VoiceWatch::new(sample_rate, &bus),
        })
    })
}

/// The safety limiter on the sum of all sources.
struct MasterProcessor(MasterBus);

impl Processor for MasterProcessor {
    #[inline]
    fn process(&mut self, input: &[f32], output: &mut [f32], _sidechain_peak: f32) {
        self.0.process(input, output);
    }
}

pub fn master_factory() -> ProcessorFactory {
    Arc::new(|sample_rate| {
        log::debug!("building the master bus at {sample_rate} Hz");
        Box::new(MasterProcessor(MasterBus::new(sample_rate)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidestage_dsp::{Preset, Settings};

    const SR: u32 = 48_000;
    const BLOCK: usize = 480;

    fn block(level: f32) -> Vec<f32> {
        (0..BLOCK * 2).map(|i| if (i / 2) % 2 == 0 { level } else { -level }).collect()
    }

    #[test]
    fn factory_builds_working_processor() {
        let f = source_factory(SharedParams::new(Settings::default()), VoiceBus::new(), Some(0));
        let mut p = f(SR);
        let input = vec![0.25f32; 2 * BLOCK];
        let mut output = vec![0.0f32; 2 * BLOCK];
        p.process(&input, &mut output, 0.0);
        assert!(output.iter().all(|s| s.is_finite()));
        let mut master = master_factory()(SR);
        let loud = vec![3.0f32; 2 * BLOCK];
        master.process(&loud, &mut output, 0.0);
        assert!(output.iter().all(|s| s.abs() <= 1.0), "the master bus never clips");
    }

    #[test]
    fn only_voice_chat_publishes_and_nobody_hears_itself() {
        let bus = VoiceBus::new();
        let voice = SharedParams::new(Preset::VoiceRight.settings());
        let music = SharedParams::new(Preset::BehindYou.settings());
        let mut discord = source_factory(Arc::clone(&voice), Arc::clone(&bus), Some(0))(SR);
        let mut spotify = source_factory(Arc::clone(&music), Arc::clone(&bus), Some(1))(SR);
        let mut out = vec![0.0; BLOCK * 2];

        let mut watch = VoiceWatch::new(SR, &bus);
        discord.process(&block(0.5), &mut out, 0.0);
        spotify.process(&block(0.9), &mut out, 0.0);
        assert_eq!(watch.loudest_other(&bus, None, BLOCK as u32), 0.5, "the voice level is its input peak");
        assert_eq!(watch.loudest_other(&bus, Some(0), BLOCK as u32), 0.0, "a source never ducks under itself");

        // The music turned into voice chat publishes too; voice chat turned off stops.
        music.set(Settings { voice_chat: true, ..Preset::BehindYou.settings() });
        voice.set(Settings { voice_chat: false, ..Preset::VoiceRight.settings() });
        discord.process(&block(0.5), &mut out, 0.0);
        spotify.process(&block(0.9), &mut out, 0.0);
        assert_eq!(watch.loudest_other(&bus, None, BLOCK as u32), 0.9);
        assert_eq!(watch.loudest_other(&bus, Some(1), BLOCK as u32), 0.0);
    }

    #[test]
    fn removed_or_silent_writers_stop_counting() {
        let bus = VoiceBus::new();
        let mut watch = VoiceWatch::new(SR, &bus);
        let voice = SharedParams::new(Preset::VoiceLeft.settings());
        let mut out = vec![0.0; BLOCK * 2];
        let mut discord = source_factory(Arc::clone(&voice), Arc::clone(&bus), Some(3))(SR);
        discord.process(&block(0.4), &mut out, 0.0);
        assert_eq!(watch.loudest_other(&bus, Some(0), BLOCK as u32), 0.4);
        drop(discord);
        assert_eq!(watch.loudest_other(&bus, Some(0), BLOCK as u32), 0.0, "dropping clears the slot");

        // A writer the backend stops calling (without dropping it) goes stale.
        bus.publish(5, 0.7);
        assert_eq!(watch.loudest_other(&bus, None, BLOCK as u32), 0.7);
        let stale_blocks = (SR as f32 * STALE_SECONDS) as usize / BLOCK;
        for _ in 0..stale_blocks {
            watch.loudest_other(&bus, None, BLOCK as u32);
        }
        assert_eq!(watch.loudest_other(&bus, None, BLOCK as u32), 0.0);
        // It counts again as soon as it publishes.
        bus.publish(5, 0.6);
        assert_eq!(watch.loudest_other(&bus, None, BLOCK as u32), 0.6);
        // A reader built later ignores values left on the bus from before it existed.
        assert_eq!(VoiceWatch::new(SR, &bus).loudest_other(&bus, None, BLOCK as u32), 0.0);
    }

    #[test]
    fn voice_chat_ducks_the_music() {
        let bus = VoiceBus::new();
        let music = SharedParams::new(Preset::BehindYou.settings());
        assert!(music.get().ducking.under_voice);
        let mut spotify = source_factory(Arc::clone(&music), Arc::clone(&bus), Some(1))(SR);
        let mut discord =
            source_factory(SharedParams::new(Preset::VoiceRight.settings()), Arc::clone(&bus), Some(0))(SR);
        let mut out = vec![0.0; BLOCK * 2];
        // One second of talking (-6 dBFS) over the music, no game.
        for _ in 0..100 {
            discord.process(&block(0.5), &mut out, 0.0);
            spotify.process(&block(0.3), &mut out, 0.0);
        }
        let ducked = music.meters.take().duck_db;
        assert!(ducked > 3.0, "the music turns down while voice chat talks: {ducked} dB");

        // Under-voice off: the same talking leaves the music alone.
        music.set(Settings {
            ducking: sidestage_dsp::Ducking { under_voice: false, ..music.get().ducking },
            ..music.get()
        });
        for _ in 0..300 {
            discord.process(&block(0.5), &mut out, 0.0);
            spotify.process(&block(0.3), &mut out, 0.0);
        }
        let ducked = music.meters.take().duck_db;
        assert!(ducked < 0.5, "under_voice off: {ducked} dB");
    }

    #[test]
    fn slots_are_reused_as_late_as_possible() {
        let mut slots = VoiceSlots::default();
        assert_eq!(slots.assign(10), Some(0));
        assert_eq!(slots.assign(11), Some(1));
        assert_eq!(slots.assign(10), Some(0), "a source keeps its slot");
        slots.retain(|id| id != 10);
        assert_eq!(slots.slot_of(10), None);
        // Slot 0 was freed, but every never-used slot goes first.
        let fresh: Vec<_> = (20..20 + VOICE_SLOTS as u32 - 2).map(|id| slots.assign(id).unwrap()).collect();
        assert!(!fresh.contains(&0));
        assert_eq!(slots.assign(99), Some(0));
        assert_eq!(slots.assign(100), None, "all slots taken");
    }
}
