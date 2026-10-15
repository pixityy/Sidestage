//! `Spatializer::process` never touches the heap, whatever the settings do.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use sidestage_dsp::{DuckMode, Preset, Room, Settings};

/// Counts allocations made by the current thread while tracking is on (other test-harness
/// threads may allocate freely).
struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static TRACKING: Cell<bool> = const { Cell::new(false) };
}

fn note() {
    if TRACKING.try_with(Cell::get).unwrap_or(false) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn process_does_not_allocate() {
    for fs in [22_050, 48_000, 192_000] {
        let (mut sp, params) = spatializer(Preset::BehindYou.settings(), fs);
        // Everything the loop below needs is allocated up front.
        let mut rng = Rng::new(fs as u64);
        let input: Vec<f32> = (0..2 * 4096).map(|_| 0.5 * rng.white()).collect();
        let mut output = vec![0.0f32; input.len()];
        let mut scenarios: Vec<Settings> = Preset::ALL.iter().map(|p| p.settings()).collect();
        let base = Preset::BehindYou.settings();
        scenarios.push(Settings { bypass: true, ..base });
        scenarios.push(Settings { room: Room::Off, ducking: DuckMode::Competitive.ducking(), ..base });
        scenarios.push(Settings { azimuth_deg: f32::NAN, distance_m: f32::INFINITY, ..base });
        let lengths = [0usize, 1, 2, 7, 128, 961, 2048, 8192];

        ALLOCS.store(0, Ordering::Relaxed);
        TRACKING.with(|t| t.set(true));
        for round in 0..400usize {
            if round.is_multiple_of(5) {
                params.set(scenarios[(round / 5) % scenarios.len()]);
            }
            let len = lengths[round % lengths.len()];
            let sidechain = if round.is_multiple_of(3) { 0.5 } else { 0.0 };
            sp.process(&input[..len], &mut output[..len], sidechain);
            let _ = params.meters.take();
        }
        TRACKING.with(|t| t.set(false));
        assert_eq!(ALLOCS.load(Ordering::Relaxed), 0, "process allocated at {fs} Hz");
    }
}
