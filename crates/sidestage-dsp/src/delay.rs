//! Delay lines: a power-of-two ring of past input, read at integer or fractional delays.

/// Ring buffer of past samples. A block is written first, then read relative to the position
/// of each of its samples, so reads of delay >= 0 only ever see already-written data.
#[derive(Clone, Debug)]
pub struct DelayLine {
    buf: Box<[f32]>,
    mask: usize,
    next: usize,
}

impl DelayLine {
    /// Holds at least `min_len` samples (a block plus the longest delay plus interpolation taps).
    pub fn new(min_len: usize) -> Self {
        let len = min_len.max(4).next_power_of_two();
        Self { buf: vec![0.0; len].into_boxed_slice(), mask: len - 1, next: 0 }
    }

    /// Appends `x` and returns the position of `x[0]`; sample `i` of the block is at
    /// `pos.wrapping_add(i)`.
    #[inline]
    pub fn write(&mut self, x: &[f32]) -> usize {
        let start = self.next;
        for &v in x {
            self.buf[self.next] = v;
            self.next = (self.next + 1) & self.mask;
        }
        start
    }

    /// The sample written `delay` samples before the one at `pos`.
    #[inline]
    pub fn read(&self, pos: usize, delay: usize) -> f32 {
        self.buf[pos.wrapping_sub(delay) & self.mask]
    }

    /// Value `delay` samples before `pos`, interpolated with a 3rd-order Lagrange polynomial
    /// through the neighbouring samples at delays d-1, d, d+1, d+2 (d = floor(delay)). Unlike
    /// linear interpolation (-6 dB at 16 kHz for a half-sample offset at 48 kHz) it keeps the
    /// treble nearly flat, so a fractional ITD does not create a spurious level difference.
    /// `delay` must be >= 1.
    #[inline]
    pub fn read_lagrange3(&self, pos: usize, delay: f32) -> f32 {
        let d0 = delay as usize;
        let f = delay - d0 as f32;
        let i = pos.wrapping_sub(d0);
        let m = self.mask;
        let xm1 = self.buf[i.wrapping_add(1) & m];
        let x0 = self.buf[i & m];
        let x1 = self.buf[i.wrapping_sub(1) & m];
        let x2 = self.buf[i.wrapping_sub(2) & m];
        let (fp1, fm1, fm2) = (f + 1.0, f - 1.0, f - 2.0);
        let hm1 = -f * fm1 * fm2 * (1.0 / 6.0);
        let h0 = fp1 * fm1 * fm2 * 0.5;
        let h1 = -fp1 * f * fm2 * 0.5;
        let h2 = fp1 * f * fm1 * (1.0 / 6.0);
        hm1 * xm1 + h0 * x0 + h1 * x1 + h2 * x2
    }

    pub fn clear(&mut self) {
        self.buf.fill(0.0);
    }

    /// Copies another line's contents (same capacity; no allocation).
    pub fn copy_from(&mut self, other: &DelayLine) {
        if self.buf.len() == other.buf.len() {
            self.buf.copy_from_slice(&other.buf);
            self.next = other.next;
        } else {
            self.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_and_fractional_reads() {
        let mut d = DelayLine::new(64);
        let ramp: Vec<f32> = (0..40).map(|i| i as f32).collect();
        let pos = d.write(&ramp);
        let last = pos.wrapping_add(39);
        assert_eq!(d.read(last, 0), 39.0);
        assert_eq!(d.read(last, 5), 34.0);
        // Lagrange is exact on polynomials up to degree 3.
        assert!((d.read_lagrange3(last, 2.25) - 36.75).abs() < 1e-4);
        assert!((d.read_lagrange3(last, 7.0) - 32.0).abs() < 1e-6);
    }

    #[test]
    fn lagrange_keeps_treble() {
        // 12 kHz at 48 kHz through a half-sample delay: Lagrange-3 loses about 1 dB.
        let fs = 48_000.0;
        let mut d = DelayLine::new(256);
        let w = 2.0 * std::f32::consts::PI * 12_000.0 / fs;
        let (mut e_in, mut e_out) = (0.0f32, 0.0f32);
        for i in 0..4000 {
            let x = (w * i as f32).sin();
            let pos = d.write(&[x]);
            if i > 100 {
                e_in += x * x;
                e_out += d.read_lagrange3(pos, 3.5).powi(2);
            }
        }
        let gain = (e_out / e_in).sqrt();
        assert!(gain > 0.87, "{gain}");
    }
}
