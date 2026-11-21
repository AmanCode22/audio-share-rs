//! Output sinks: where received PCM goes.
//!
//! - [`NullSink`] — counts bytes, used by tests and headless checks.
//! - [`CpalSink`] — real audio playback with a jitter buffer, resampling and
//!   channel adaptation (feature `audio`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;

use crate::protocol::AudioFormat;

/// Byte-oriented jitter buffer shared by sinks: absorbs network timing
/// variation, drops the oldest data when the producer runs ahead, and
/// reports the buffered amount so the UI can show latency.
#[derive(Default)]
pub struct Jitter {
    buf: VecDeque<u8>,
    bytes_per_second: usize,
    dropped_backlog: AtomicU64,
}

impl Jitter {
    pub fn new(bytes_per_second: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            bytes_per_second,
            dropped_backlog: AtomicU64::new(0),
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend(data.iter().copied());
    }

    /// Pop up to `want` bytes (all-or-nothing frames are not required here —
    /// the sink guarantees frame alignment before decoding).
    pub fn pop(&mut self, want: usize) -> usize {
        let n = want.min(self.buf.len());
        for _ in 0..n {
            self.buf.pop_front();
        }
        n
    }

    pub fn peek(&self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.buf.len());
        for (i, b) in self.buf.iter().take(n).enumerate() {
            out[i] = *b;
        }
        n
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn buffered_ms(&self) -> u32 {
        if self.bytes_per_second == 0 {
            0
        } else {
            (self.buf.len() as u64 * 1000 / self.bytes_per_second as u64) as u32
        }
    }

    /// Drop backlog keeping the newest `keep` bytes; returns dropped count.
    pub fn resync(&mut self, keep: usize) -> u64 {
        if self.buf.len() <= keep {
            return 0;
        }
        let drop = self.buf.len() - keep;
        for _ in 0..drop {
            self.buf.pop_front();
        }
        self.dropped_backlog
            .fetch_add(drop as u64, Ordering::Relaxed);
        drop as u64
    }

    pub fn dropped_backlog(&self) -> u64 {
        self.dropped_backlog.load(Ordering::Relaxed)
    }
}

/// A playback endpoint. `push` is called from network tasks; implementors
/// must buffer internally and stay cheap.
pub trait PcmSink: Send + 'static {
    fn push(&mut self, seq: u32, pcm: &[u8]);
    fn buffered_ms(&self) -> u32 {
        0
    }
    /// Hint for the UI: bytes consumed so far.
    fn consumed_bytes(&self) -> u64 {
        0
    }
    fn shutdown(&mut self) {}
}

/// Test/headless sink: swallows bytes, counts them.
pub struct NullSink {
    consumed: Arc<AtomicU64>,
}

impl NullSink {
    pub fn new() -> Self {
        Self {
            consumed: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn consumed(&self) -> u64 {
        self.consumed.load(Ordering::Relaxed)
    }
}

impl Default for NullSink {
    fn default() -> Self {
        Self::new()
    }
}

impl PcmSink for NullSink {
    fn push(&mut self, _seq: u32, pcm: &[u8]) {
        self.consumed.fetch_add(pcm.len() as u64, Ordering::Relaxed);
    }
}

/// Builds a [`PcmSink`] for a negotiated stream format.
pub trait SinkFactory: Send + Sync + 'static {
    fn build(&self, format: &AudioFormat) -> Result<Box<dyn PcmSink>>;
}

/// Factory for [`NullSink`].
#[derive(Default, Clone)]
pub struct NullSinkFactory;

impl SinkFactory for NullSinkFactory {
    fn build(&self, _format: &AudioFormat) -> Result<Box<dyn PcmSink>> {
        Ok(Box::new(NullSink::new()))
    }
}

#[cfg(feature = "audio")]
pub mod cpal_sink {
    //! Real playback via cpal with jitter buffering, linear resampling and
    //! channel adaptation. Latency ≈ `buffer_ms` target + device period.
    //! The cpal stream lives on the feeder thread (Stream is !Send).

    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{bail, Context, Result};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    use super::{Jitter, PcmSink, SinkFactory};
    use crate::capture::convert::adapt_channels;
    use crate::client::resample::LinearResampler;
    use crate::protocol::{AudioFormat, Encoding};

    pub struct CpalSinkFactory {
        /// Output device name substring; None = system default.
        pub device: Option<String>,
        /// Target jitter buffer in milliseconds.
        pub buffer_ms: u32,
        /// Software output volume 0.0..=2.0.
        pub volume: f32,
    }

    pub struct CpalSink {
        #[allow(dead_code)]
        format: AudioFormat,
        jitter: Arc<Mutex<Jitter>>,
        pushed: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
        feeder: Option<std::thread::JoinHandle<()>>,
    }

    struct SharedOut {
        ring: Mutex<VecDeque<f32>>,
        underruns: AtomicU64,
        volume: Mutex<f32>,
    }

    impl SinkFactory for CpalSinkFactory {
        fn build(&self, format: &AudioFormat) -> Result<Box<dyn PcmSink>> {
            let host = cpal::default_host();
            let device = match &self.device {
                Some(name) => {
                    let needle = name.to_lowercase();
                    host.output_devices()?
                        .find(|d| {
                            d.name()
                                .map(|n| n.to_lowercase().contains(&needle))
                                .unwrap_or(false)
                        })
                        .with_context(|| format!("output device matching '{name}' not found"))?
                }
                None => host
                    .default_output_device()
                    .context("no default output device")?,
            };
            let dev_name = device.name().unwrap_or_default().to_string();

            // Prefer the server's rate/channels when supported.
            let supported: Vec<cpal::SupportedStreamConfigRange> =
                device.supported_output_configs()?.collect();
            let want_ch = format.channels;
            let cfg = supported
                .iter()
                .find(|r| {
                    r.channels() == want_ch
                        && r.min_sample_rate().0 <= format.sample_rate
                        && r.max_sample_rate().0 >= format.sample_rate
                })
                .map(|r| r.with_sample_rate(cpal::SampleRate(format.sample_rate)))
                .or_else(|| {
                    supported
                        .iter()
                        .find(|r| r.channels() == want_ch)
                        .map(|r| r.with_max_sample_rate())
                })
                .or_else(|| {
                    supported
                        .iter()
                        .find(|r| r.channels() == 2)
                        .map(|r| r.with_max_sample_rate())
                })
                .or_else(|| {
                    supported
                        .iter()
                        .find(|r| r.channels() == 1)
                        .map(|r| r.with_max_sample_rate())
                })
                .or_else(|| device.default_output_config().ok())
                .context("no supported output config")?;

            let dev_channels = cfg.channels();
            let dev_rate = cfg.sample_rate().0;
            let sample_format = cfg.sample_format();
            let stream_cfg = cpal::StreamConfig {
                channels: dev_channels,
                sample_rate: cpal::SampleRate(dev_rate),
                buffer_size: cpal::BufferSize::Default,
            };

            let jitter = Arc::new(Mutex::new(Jitter::new(format.bytes_per_second())));
            let ring: Arc<SharedOut> = Arc::new(SharedOut {
                ring: Mutex::new(VecDeque::with_capacity(dev_rate as usize / 2 + 4096)),
                underruns: AtomicU64::new(0),
                volume: Mutex::new(self.volume.max(0.0)),
            });

            let stop = Arc::new(AtomicBool::new(false));

            // Feeder thread owns the cpal stream (!Send) and pulls from the
            // jitter buffer into the callback ring.
            let stop_for_feeder = Arc::clone(&stop);
            let jitter_for_feeder = Arc::clone(&jitter);
            let ring_for_feeder = Arc::clone(&ring);
            let in_rate = format.sample_rate;
            let in_channels = format.channels as usize;
            let target_ms = self.buffer_ms.max(20);
            let feeder_format = *format;
            let feeder = std::thread::Builder::new()
                .name("ash-playback".into())
                .spawn(move || {
                    let run = || -> Result<()> {
                        let host = cpal::default_host();
                        let device = match dev_name.is_empty() {
                            true => host.default_output_device().context("no output device")?,
                            false => host
                                .output_devices()?
                                .find(|d| d.name().map(|n| n == dev_name).unwrap_or(false))
                                .context("output device vanished")?,
                        };
                        let err_fn = |e| tracing::warn!("output stream error: {e}");
                        let ring = Arc::clone(&ring_for_feeder);
                        let stream = match sample_format {
                            cpal::SampleFormat::F32 => device.build_output_stream(
                                &stream_cfg,
                                move |data: &mut [f32], _| {
                                    let (mut v, miss) = pop_frames(&ring, data.len());
                                    if miss > 0 {
                                        ring.underruns.fetch_add(1, Ordering::Relaxed);
                                    }
                                    let vol = *ring.volume.lock().unwrap();
                                    for (d, s) in data.iter_mut().zip(v.drain(..)) {
                                        *d = s * vol;
                                    }
                                },
                                err_fn,
                                None,
                            )?,
                            cpal::SampleFormat::I16 => device.build_output_stream(
                                &stream_cfg,
                                move |data: &mut [i16], _| {
                                    let (mut v, miss) = pop_frames(&ring, data.len());
                                    if miss > 0 {
                                        ring.underruns.fetch_add(1, Ordering::Relaxed);
                                    }
                                    let vol = *ring.volume.lock().unwrap();
                                    for (d, s) in data.iter_mut().zip(v.drain(..)) {
                                        *d = ((s * vol).clamp(-1.0, 1.0) * 32767.0).round() as i16;
                                    }
                                },
                                err_fn,
                                None,
                            )?,
                            cpal::SampleFormat::U8 => device.build_output_stream(
                                &stream_cfg,
                                move |data: &mut [u8], _| {
                                    let (mut v, miss) = pop_frames(&ring, data.len());
                                    if miss > 0 {
                                        ring.underruns.fetch_add(1, Ordering::Relaxed);
                                    }
                                    let vol = *ring.volume.lock().unwrap();
                                    for (d, s) in data.iter_mut().zip(v.drain(..)) {
                                        *d = (((s * vol).clamp(-1.0, 1.0) * 127.0) + 128.0).round()
                                            as u8;
                                    }
                                },
                                err_fn,
                                None,
                            )?,
                            other => bail!("unsupported output sample format {other:?}"),
                        };
                        stream.play()?;
                        tracing::info!("playback started ({} Hz, {} ch)", dev_rate, dev_channels);

                        // Feed loop: jitter -> f32 -> channel adapt -> resample -> ring
                        let mut resampler = LinearResampler::new(in_channels, in_rate, dev_rate);
                        let needs_resample = in_rate != dev_rate;
                        let bps = feeder_format.bytes_per_second();
                        let bpf = feeder_format.bytes_per_frame().max(1);
                        let pull_bytes = ((bps / 1000 * 10 / 2) / bpf) * bpf;
                        let mut drain: VecDeque<u8> = VecDeque::new();
                        while !stop_for_feeder.load(Ordering::Relaxed) {
                            {
                                let mut j = jitter_for_feeder.lock().unwrap();
                                let buffered = j.buffered_ms();
                                if buffered > target_ms * 3 {
                                    j.resync(target_ms as usize * 2);
                                }
                                if drain.len() < pull_bytes {
                                    let mut raw = vec![0u8; pull_bytes];
                                    let got = j.peek(&mut raw);
                                    let aligned = got - (got % bpf);
                                    raw.truncate(aligned);
                                    j.pop(aligned);
                                    drain.extend(raw);
                                }
                            }
                            if drain.len() < bpf {
                                std::thread::sleep(Duration::from_millis(5));
                                continue;
                            }
                            let take = drain.len() - (drain.len() % bpf);
                            let bytes: Vec<u8> = drain.drain(..take).collect();
                            let samples: Vec<f32> = match feeder_format.encoding {
                                Encoding::F32Le => bytes
                                    .chunks_exact(4)
                                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                                    .collect(),
                                Encoding::S16Le => bytes
                                    .chunks_exact(2)
                                    .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                                    .collect(),
                            };
                            let adapted =
                                adapt_channels(&samples, in_channels, dev_channels as usize);
                            let out: Vec<f32> = if needs_resample {
                                resampler.process(&adapted)
                            } else {
                                adapted
                            };
                            {
                                let mut q = ring_for_feeder.ring.lock().unwrap();
                                let cap = dev_rate as usize / 2;
                                if q.len() > cap {
                                    let excess = q.len() - cap;
                                    for _ in 0..excess {
                                        q.pop_front();
                                    }
                                }
                                q.extend(out);
                            }
                        }
                        Ok(())
                    };
                    if let Err(e) = run() {
                        tracing::error!("playback thread failed: {e:#}");
                    }
                    // stream dropped -> playback stops
                })
                .context("spawn playback thread")?;

            Ok(Box::new(CpalSink {
                format: *format,
                jitter,
                pushed: Arc::new(AtomicU64::new(0)),
                stop,
                feeder: Some(feeder),
            }))
        }
    }

    impl PcmSink for CpalSink {
        fn push(&mut self, _seq: u32, pcm: &[u8]) {
            self.pushed.fetch_add(pcm.len() as u64, Ordering::Relaxed);
            if let Ok(mut j) = self.jitter.lock() {
                j.push(pcm);
            }
        }

        fn buffered_ms(&self) -> u32 {
            self.jitter.lock().map(|j| j.buffered_ms()).unwrap_or(0)
        }

        fn consumed_bytes(&self) -> u64 {
            self.pushed.load(Ordering::Relaxed)
        }

        fn shutdown(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.feeder.take() {
                let _ = t.join();
            }
            if let Ok(mut j) = self.jitter.lock() {
                j.resync(0);
            }
        }
    }

    impl Drop for CpalSink {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    fn pop_frames(ring: &SharedOut, out_len: usize) -> (Vec<f32>, usize) {
        let mut q = match ring.ring.try_lock() {
            Ok(q) => q,
            Err(_) => return (vec![0.0; out_len], out_len),
        };
        if q.len() < out_len {
            let missing = out_len - q.len();
            let mut v: Vec<f32> = q.drain(..).collect();
            v.resize(out_len, 0.0);
            return (v, missing);
        }
        (q.drain(..out_len).collect(), 0)
    }
}

#[cfg(feature = "audio")]
pub use cpal_sink::{CpalSink, CpalSinkFactory};

/// Playback devices available to cpal (for device pickers).
#[cfg(feature = "audio")]
pub fn list_output_devices() -> anyhow::Result<Vec<crate::capture::DeviceInfo>> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let default_name = host
        .default_output_device()
        .and_then(|d| d.name().ok())
        .map(|s| s.to_lowercase());
    let mut out = Vec::new();
    for d in host.output_devices()? {
        let Ok(cfg) = d.default_output_config() else {
            continue;
        };
        let name = d.name().unwrap_or_default().to_string();
        out.push(crate::capture::DeviceInfo {
            is_default: default_name
                .as_ref()
                .map(|def| name.to_lowercase() == *def)
                .unwrap_or(false),
            sample_rate: cfg.sample_rate().0,
            channels: cfg.channels(),
            name,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_basic() {
        let mut j = Jitter::new(192_000); // 2 ch * 2 B * 48 kHz
        j.push(&vec![0u8; 1920]); // 10 ms
        assert_eq!(j.buffered_ms(), 10);
        assert_eq!(j.pop(1000), 1000);
        assert_eq!(j.len(), 920);
        j.push(&vec![0u8; 5000]);
        assert_eq!(j.resync(1000), 5920 - 1000);
    }
}
