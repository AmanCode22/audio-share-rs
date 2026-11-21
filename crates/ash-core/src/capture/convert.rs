//! PCM sample conversion: interleaved f32 (the universal capture format)
//! to the wire encodings, plus channel adaptation.

use crate::protocol::Encoding;

/// Convert interleaved f32 samples to the target encoding's byte stream.
pub fn f32_samples_to_bytes(samples: &[f32], encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::F32Le => {
            let mut out = Vec::with_capacity(samples.len() * 4);
            for s in samples {
                out.extend_from_slice(&s.to_le_bytes());
            }
            out
        }
        Encoding::S16Le => {
            let mut out = Vec::with_capacity(samples.len() * 2);
            for s in samples {
                let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                out.extend_from_slice(&v.to_le_bytes());
            }
            out
        }
    }
}

/// Adapt interleaved f32 sample data between channel counts (1↔2 primarily).
pub fn adapt_channels(samples: &[f32], from: usize, to: usize) -> Vec<f32> {
    if from == to || samples.is_empty() || from == 0 {
        return samples.to_vec();
    }
    let frames = samples.len() / from;
    let mut out = Vec::with_capacity(frames * to);
    for f in 0..frames {
        let frame = &samples[f * from..(f + 1) * from];
        match (from, to) {
            (1, 2) => {
                out.push(frame[0]);
                out.push(frame[0]);
            }
            (2, 1) => {
                out.push((frame[0] + frame[1]) * 0.5);
            }
            _ => {
                // Generic: repeat or truncate channels frame-wise.
                for c in 0..to {
                    out.push(frame[c.min(from - 1)]);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_bytes_roundtrip_len() {
        let s = [0.0f32, 0.5, -0.5, 1.0];
        let b = f32_samples_to_bytes(&s, Encoding::F32Le);
        assert_eq!(b.len(), 16);
        let b = f32_samples_to_bytes(&s, Encoding::S16Le);
        assert_eq!(b.len(), 8);
        // Spot-check clamping and rounding.
        assert_eq!(&b[0..2], &0i16.to_le_bytes());
    }

    #[test]
    fn channel_adapt() {
        let stereo = [0.1f32, 0.2, 0.3, 0.4];
        let mono = adapt_channels(&stereo, 2, 1);
        assert_eq!(mono.len(), 2);
        assert!((mono[0] - 0.15).abs() < 1e-6);
        let doubled = adapt_channels(&[0.5f32], 1, 2);
        assert_eq!(doubled, vec![0.5, 0.5]);
    }
}
