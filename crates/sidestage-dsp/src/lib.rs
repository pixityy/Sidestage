//! Sidestage's binaural renderer.
//!
//! Takes the captured stereo music and makes it sound like it is playing somewhere around the
//! listener's head (by default: behind them, a little distant, in a small room), for headphones.
//!
//! Public API (stable; the app depends on it):
//! - [`Settings`]: every user-facing parameter, serializable, with [`Preset`]s and [`DuckMode`]s.
//! - [`SharedParams`]: lock-free hand-off of `Settings` from the UI thread to the audio thread,
//!   plus [`Meters`] going the other way.
//! - [`Spatializer`]: one source's real-time renderer.
//! - [`MasterBus`]: the safety limiter on the mix of all sources.

mod biquad;
mod delay;
mod ducker;
mod head;
mod leveler;
mod limiter;
mod master;
mod params;
mod presets;
mod renderer;
mod reverb;
mod smoothing;
mod voice;

pub use master::MasterBus;
pub use params::{Ducking, MeterReading, Meters, Room, Settings, SharedParams};
pub use presets::{DuckMode, Preset, PresetKind, make_footstep_safe};
pub use renderer::{Spatializer, render_offline};
