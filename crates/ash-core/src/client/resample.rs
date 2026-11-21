//! Linear-interpolation resampler for rate conversion (e.g. 48 kHz server →
//! 44.1 kHz device). Deliberately dependency-free so the wire logic can be
//! tested everywhere; quality is fine for speech/music streaming.

/// Interleaved float resampler operating frame-wise.
pub struct LinearResampler {
    channels: usize,
    /// input frames consumed per output frame
    ratio: f64,
    /// fractional position between `prev` and the next input frame
    frac: f64,
    prev: Vec<f32>,
}

impl LinearResampler {
    pub fn new(channels: usize, in_rate: u32, out_rate: u32) -> Self {
        Self {
            channels: channels.max(1),
            ratio: in_rate as f64 / out_rate as f64,
            frac: 0.0,
            prev: vec![0.0; channels.max(1)],
        }
    }

    /// Resample `input` (interleaved f32, multiple of `channels`).
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let ch = self.channels;
        let frames = input.len() / ch;
        let mut out = Vec::with_capacity((frames as f64 / self.ratio + 2.0) as usize * ch);
        if frames == 0 {
            return out;
        }
        let mut frame_idx = 0usize;
        loop {
            self.frac += self.ratio;
            while self.frac >= 1.0 && frame_idx < frames {
                // shift one input frame into prev
                let base = frame_idx * ch;
                self.prev.copy_from_slice(&input[base..base + ch]);
                self.frac -= 1.0;
                frame_idx += 1;
            }
            if frame_idx >= frames {
                break;
            }
            // interpolate prev -> current frame at frac
            let base = frame_idx * ch;
            for c in 0..ch {
                let a = self.prev[c];
                let b = input[base + c];
                out.push(a + (b - a) * self.frac as f32);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_holds() {
        // 48000 -> 44100: 4800 in-frames should yield ~4410 out-frames.
        let mut r = LinearResampler::new(2, 48000, 44100);
        let input = vec![0.0f32; 4800 * 2];
        let out = r.process(&input);
        let out_frames = out.len() / 2;
        assert!((out_frames as i64 - 4410).abs() < 40, "got {out_frames}");
    }

    #[test]
    fn dc_passes() {
        let mut r = LinearResampler::new(1, 44100, 48000);
        let input = vec![0.5f32; 4410];
        let out = r.process(&input);
        assert!(!out.is_empty());
        let tail = &out[out.len() - 64..];
        for s in tail {
            assert!((s - 0.5).abs() < 0.01, "s = {s}");
        }
    }
}
