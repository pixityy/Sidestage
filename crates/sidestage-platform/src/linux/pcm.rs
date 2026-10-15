//! Decoding float32le sample data from the socket.
//!
//! Stream data arrives in packets whose payload is read through a fixed-size buffer, so one
//! frame can be split across two reads. The decoder carries the partial frame over. It never
//! allocates after construction, so it is safe on the audio thread.

pub struct FrameDecoder {
    frame_bytes: usize,
    carry: [u8; 16],
    carry_len: usize,
    scratch: Vec<f32>,
}

impl FrameDecoder {
    /// `channels` per frame (at most 4), decoded in blocks of up to `block_frames` frames.
    pub fn new(channels: usize, block_frames: usize) -> Self {
        assert!((1..=4).contains(&channels) && block_frames > 0);
        Self { frame_bytes: channels * 4, carry: [0; 16], carry_len: 0, scratch: vec![0.0; channels * block_frames] }
    }

    /// Drops a partial frame (e.g. at the end of a packet, where none should be left).
    pub fn reset(&mut self) {
        self.carry_len = 0;
    }

    /// Decodes `bytes`, handing whole interleaved frames to `sink` in blocks. Returns the
    /// largest absolute sample value seen.
    pub fn feed(&mut self, mut bytes: &[u8], mut sink: impl FnMut(&[f32])) -> f32 {
        let fb = self.frame_bytes;
        let mut peak = 0.0f32;
        if self.carry_len > 0 {
            let n = (fb - self.carry_len).min(bytes.len());
            self.carry[self.carry_len..self.carry_len + n].copy_from_slice(&bytes[..n]);
            self.carry_len += n;
            bytes = &bytes[n..];
            if self.carry_len < fb {
                return peak;
            }
            let ch = fb / 4;
            for (dst, src) in self.scratch[..ch].iter_mut().zip(self.carry[..fb].as_chunks::<4>().0) {
                *dst = f32::from_le_bytes(*src);
                peak = peak.max(dst.abs());
            }
            sink(&self.scratch[..ch]);
            self.carry_len = 0;
        }
        let block_bytes = self.scratch.len() * 4;
        let whole = bytes.len() - bytes.len() % fb;
        for chunk in bytes[..whole].chunks(block_bytes) {
            let n = chunk.len() / 4;
            for (dst, src) in self.scratch[..n].iter_mut().zip(chunk.as_chunks::<4>().0) {
                *dst = f32::from_le_bytes(*src);
                peak = peak.max(dst.abs());
            }
            sink(&self.scratch[..n]);
        }
        let rest = &bytes[whole..];
        self.carry[..rest.len()].copy_from_slice(rest);
        self.carry_len = rest.len();
        peak
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn reassembles_frames_split_anywhere() {
        let samples: Vec<f32> = (0..64).map(|i| i as f32 * if i % 2 == 0 { 0.01 } else { -0.01 }).collect();
        let bytes = encode(&samples);
        for split in [1, 3, 5, 7, 8, 9, 13, 64, 100, 255] {
            let mut d = FrameDecoder::new(2, 4);
            let mut out = Vec::new();
            let mut peak = 0.0f32;
            for part in bytes.chunks(split) {
                peak = peak.max(d.feed(part, |frames| {
                    assert_eq!(frames.len() % 2, 0, "whole stereo frames only");
                    out.extend_from_slice(frames);
                }));
            }
            assert_eq!(out, samples, "split {split}");
            assert!((peak - 0.63).abs() < 1e-6);
        }
    }

    #[test]
    fn mono_and_reset() {
        let mut d = FrameDecoder::new(1, 8);
        let mut got = Vec::new();
        assert_eq!(d.feed(&encode(&[0.5])[..2], |f| got.extend_from_slice(f)), 0.0);
        d.reset();
        let peak = d.feed(&encode(&[0.25, -0.75]), |f| got.extend_from_slice(f));
        assert_eq!(got, [0.25, -0.75]);
        assert_eq!(peak, 0.75);
    }
}
