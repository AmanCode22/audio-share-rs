//! cpal-based capture for microphone / monitor / BlackHole inputs
//! (Linux, macOS, Windows-mic fallback).
//!
//! On Linux the PipeWire/Pulse "monitor of ..." sources appear as input
//! devices, so selecting one yields true system loopback; on macOS use a
//! virtual device such as BlackHole.
//!
//! Note: `cpal::Stream` is `!Send`, so the stream is created and kept on the
//! capturer's own thread; `stop()` signals via an atomic flag.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::capture::convert::f32_samples_to_bytes;
use crate::capture::{Capturer, DeviceInfo, PcmChunk};
use crate::protocol::{AudioFormat, Encoding};

pub struct CpalCapturer {
    format: AudioFormat,
    device_name: String,
    stop: Arc<AtomicBool>,
    bridge: Option<JoinHandle<()>>,
}

impl CpalCapturer {
    /// Build a capturer on the default (or named) input device.
    /// PCM is converted to `encoding`; rate/channels follow the device.
    pub fn input(device: Option<String>, encoding: Encoding) -> Result<Self> {
        let host = cpal::default_host();
        let device = match device {
            Some(name) => find_input_device(&host, &name)
                .with_context(|| format!("input device matching '{name}' not found"))?,
            None => host
                .default_input_device()
                .context("no default input device available")?,
        };
        let name = device.name().unwrap_or_default().to_string();
        let cfg = device
            .default_input_config()
            .context("input config failed")?;
        let format = AudioFormat::new(encoding, cfg.channels(), cfg.sample_rate().0);
        Ok(Self {
            format,
            device_name: name,
            stop: Arc::new(AtomicBool::new(false)),
            bridge: None,
        })
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }
}

fn find_input_device(host: &cpal::Host, needle: &str) -> Option<cpal::Device> {
    let needle = needle.to_lowercase();
    host.devices().ok()?.find(|d| {
        d.name()
            .map(|n| n.to_lowercase().contains(&needle))
            .unwrap_or(false)
    })
}

/// Enumerate input devices (microphones and monitor sources).
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();
    let default_name = host
        .default_input_device()
        .and_then(|d| d.name().ok())
        .map(|s| s.to_lowercase());
    let mut out = Vec::new();
    for d in host.devices()? {
        let Ok(cfg) = d.default_input_config() else {
            continue;
        };
        let name = d.name().unwrap_or_default().to_string();
        out.push(DeviceInfo {
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

impl Capturer for CpalCapturer {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn start(&mut self, out: Sender<PcmChunk>) -> Result<()> {
        if self.bridge.is_some() {
            bail!("capturer already started");
        }
        let device_name = self.device_name.clone();
        let format = self.format;
        let stop = Arc::clone(&self.stop);

        // Everything audio-related happens on this thread (Stream is !Send).
        self.bridge = Some(
            std::thread::Builder::new()
                .name("ash-cpal-in".into())
                .spawn(move || {
                    let host = cpal::default_host();
                    let device = match find_input_device(&host, &device_name) {
                        Some(d) => d,
                        None => {
                            tracing::error!("device '{device_name}' disappeared before start");
                            return;
                        }
                    };
                    let cfg = match device.default_input_config() {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::error!("input config failed: {e}");
                            return;
                        }
                    };
                    let stream_cfg: cpal::StreamConfig = cfg.clone().into();
                    let enc = format.encoding;
                    let err_fn = |e| tracing::warn!("cpal input stream error: {e}");
                    let build = || -> Result<cpal::Stream> {
                        match cfg.sample_format() {
                            cpal::SampleFormat::F32 => Ok(device.build_input_stream(
                                &stream_cfg,
                                move |data: &[f32], _| {
                                    let _ = out.send(f32_samples_to_bytes(data, enc));
                                },
                                err_fn,
                                None,
                            )?),
                            cpal::SampleFormat::I16 => Ok(device.build_input_stream(
                                &stream_cfg,
                                move |data: &[i16], _| {
                                    let f: Vec<f32> =
                                        data.iter().map(|&s| s as f32 / 32768.0).collect();
                                    let _ = out.send(f32_samples_to_bytes(&f, enc));
                                },
                                err_fn,
                                None,
                            )?),
                            cpal::SampleFormat::U8 => Ok(device.build_input_stream(
                                &stream_cfg,
                                move |data: &[u8], _| {
                                    let f: Vec<f32> =
                                        data.iter().map(|&s| (s as f32 - 128.0) / 128.0).collect();
                                    let _ = out.send(f32_samples_to_bytes(&f, enc));
                                },
                                err_fn,
                                None,
                            )?),
                            other => bail!("unsupported input sample format: {other:?}"),
                        }
                    };
                    let stream = match build() {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!("failed to open input stream: {e:#}");
                            return;
                        }
                    };
                    if let Err(e) = stream.play() {
                        tracing::error!("input stream play failed: {e}");
                        return;
                    }
                    tracing::info!("capturing from '{device_name}'");
                    // Hold the stream until asked to stop.
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    // `stream` is dropped here, stopping capture.
                })
                .context("spawn cpal capture thread")?,
        );
        Ok(())
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.bridge.take() {
            let _ = t.join();
        }
    }
}

impl Drop for CpalCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}
