use std::sync::atomic::{AtomicU32, Ordering};

/// A non-negative f32 shared between threads without locks.
///
/// Non-negative IEEE-754 floats sort the same way as their bit patterns, so `fetch_max` on the
/// bits is a float max. That makes "peak since last read" meters wait-free on both sides.
#[derive(Debug, Default)]
pub struct AtomicLevel(AtomicU32);

impl AtomicLevel {
    pub const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    /// Overwrite the value. Negative and NaN inputs are stored as 0.
    #[inline]
    pub fn store(&self, v: f32) {
        self.0.store(sanitize(v).to_bits(), Ordering::Relaxed);
    }

    #[inline]
    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    /// Raise the value to at least `v` (peak-hold).
    #[inline]
    pub fn raise(&self, v: f32) {
        self.0.fetch_max(sanitize(v).to_bits(), Ordering::Relaxed);
    }

    /// Read the held peak and reset it to 0.
    #[inline]
    pub fn take(&self) -> f32 {
        f32::from_bits(self.0.swap(0, Ordering::Relaxed))
    }
}

#[inline]
fn sanitize(v: f32) -> f32 {
    // `v >= 0.0` is false for NaN; also drops -0.0, whose bit pattern would sort above +inf.
    if v > 0.0 { v.min(f32::MAX) } else { 0.0 }
}

/// Peak absolute sample value of an interleaved buffer.
#[inline]
pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raise_is_float_max() {
        let l = AtomicLevel::new();
        l.raise(0.25);
        l.raise(0.1);
        l.raise(f32::NAN);
        l.raise(-3.0);
        assert_eq!(l.load(), 0.25);
        l.raise(1.5);
        assert_eq!(l.take(), 1.5);
        assert_eq!(l.load(), 0.0);
    }

    #[test]
    fn peak_of_buffer() {
        assert_eq!(peak(&[0.1, -0.7, 0.3, 0.0]), 0.7);
        assert_eq!(peak(&[]), 0.0);
    }
}
