//! Deterministic sine-wave source — no audio hardware required.
//! Used by integration tests, CI and `--source sine` demo mode.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;

use crate::capture::convert::f32_samples_to_bytes;
use crate::capture::{Capturer, PcmChunk};
use crate::protocol::{AudioFormat, Encoding};

pub struct SineCapturer {
    format: AudioFormat,
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl SineCapturer {
    pub fn new(sample_rate: u32, channels: u16, encoding: Encoding) -> Self {
        Self {
            format: AudioFormat::new(encoding, channels.max(1), sample_rate),
            thread: None,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Capturer for SineCapturer {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn start(&mut self, out: Sender<PcmChunk>) -> Result<()> {
        let format = self.format;
        let stop = Arc::clone(&self.stop);
        self.thread = Some(std::thread::Builder::new().name("ash-sine".into()).spawn(
            move || {
                let f = format.sample_rate as f64;
                let bpf = format.bytes_per_frame();
                // Emit ~20 ms chunks.
                let frames_per_chunk = (format.sample_rate / 50).max(1) as usize;
                let mut t: u64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    let mut samples =
                        Vec::with_capacity(frames_per_chunk * format.channels as usize);
                    for i in 0..frames_per_chunk {
                        // 440 Hz + a quiet 880 Hz overtone so it sounds less shrill.
                        let x = (t + i as u64) as f64 / f;
                        let v = 0.6 * (2.0 * std::f64::consts::PI * 440.0 * x).sin()
                            + 0.2 * (2.0 * std::f64::consts::PI * 880.0 * x).sin();
                        for _ in 0..format.channels {
                            samples.push(v as f32);
                        }
                    }
                    t += frames_per_chunk as u64;
                    let bytes = f32_samples_to_bytes(&samples, format.encoding);
                    if out.send(bytes).is_err() {
                        break;
                    }
                    let dur = Duration::from_secs_f64(frames_per_chunk as f64 / f);
                    std::thread::sleep(dur);
                }
                let _ = bpf; // kept for readability
            },
        )?);
        Ok(())
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for SineCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}
