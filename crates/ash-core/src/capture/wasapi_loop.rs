//! Windows system-audio loopback capture via WASAPI (wasapi-rs crate).
//!
//! Captures whatever is played on a render endpoint ("what you hear") using
//! the WASAPI loopback flag — no virtual-audio driver needed. This is the
//! same technique the original C++ server uses.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::capture::convert::f32_samples_to_bytes;
use crate::capture::{Capturer, DeviceInfo, PcmChunk};
use crate::protocol::{AudioFormat, Encoding};

use wasapi::{DeviceEnumerator, Direction, StreamMode, WaveFormat};

pub struct WasapiLoopCapturer {
    format: AudioFormat,
    device_name: String,
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl WasapiLoopCapturer {
    /// Capture the loopback of `device` (by friendly-name substring) or of
    /// the default render endpoint. PCM is converted to `encoding`.
    pub fn new(device: Option<String>, encoding: Encoding) -> Result<Self> {
        // Resolve the endpoint up-front to know the native format.
        let enumerator = DeviceEnumerator::new().map_err(err)?;
        let (dev, name) = match &device {
            Some(needle) => {
                let needle_lc = needle.to_lowercase();
                let collection = enumerator
                    .get_device_collection(Direction::Render)
                    .map_err(err)?;
                let mut found = None;
                let count = collection.get_nbr_devices().map_err(err)?;
                for i in 0..count {
                    let d = collection.get_device_at_index(i).map_err(err)?;
                    let fname = d.get_friendlyname().map_err(err)?;
                    if fname.to_lowercase().contains(&needle_lc) {
                        found = Some((d, fname));
                        break;
                    }
                }
                found.context(format!("render endpoint matching '{needle}' not found"))?
            }
            None => {
                let d = enumerator
                    .get_default_device(Direction::Render)
                    .map_err(err)?;
                let fname = d.get_friendlyname().map_err(err)?;
                (d, fname)
            }
        };

        let audio_client = dev.get_iaudioclient().map_err(err)?;
        // The shared-mode mix format is always 32-bit float interleaved.
        let mixfmt: WaveFormat = audio_client.get_mixformat().map_err(err)?;
        let channels = mixfmt.get_nchannels() as u16;
        let sample_rate = mixfmt.get_samplespersec() as u32;
        if channels == 0 || !(8_000..=384_000).contains(&sample_rate) {
            bail!("implausible mix format: {channels} ch @ {sample_rate} Hz");
        }

        Ok(Self {
            format: AudioFormat::new(encoding, channels, sample_rate),
            device_name: name,
            thread: None,
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }
}

fn err(e: wasapi::WasapiError) -> anyhow::Error {
    anyhow::anyhow!("wasapi: {e}")
}

/// Enumerate render endpoints (loopback sources) for device pickers.
pub fn list_render_endpoints() -> Result<Vec<DeviceInfo>> {
    let enumerator = DeviceEnumerator::new().map_err(err)?;
    let collection = enumerator
        .get_device_collection(Direction::Render)
        .map_err(err)?;
    let default_name = enumerator
        .get_default_device(Direction::Render)
        .ok()
        .and_then(|d| d.get_friendlyname().ok())
        .map(|s| s.to_lowercase());
    let count = collection.get_nbr_devices().map_err(err)?;
    let mut out = Vec::new();
    for i in 0..count {
        let d = match collection.get_device_at_index(i).map_err(err) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let name = d.get_friendlyname().map_err(err)?;
        out.push(DeviceInfo {
            is_default: default_name
                .as_ref()
                .map(|def| name.to_lowercase() == *def)
                .unwrap_or(false),
            sample_rate: 0,
            channels: 0,
            name,
        });
    }
    Ok(out)
}

impl Capturer for WasapiLoopCapturer {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn start(&mut self, out: Sender<PcmChunk>) -> Result<()> {
        if self.thread.is_some() {
            bail!("capturer already started");
        }
        let format = self.format;
        let device_name = self.device_name.clone();
        let stop = Arc::clone(&self.stop);

        self.thread = Some(
            std::thread::Builder::new()
                .name("ash-wasapi-loop".into())
                .spawn(move || {
                    if let Err(e) = capture_loop(device_name, format, out, &stop) {
                        tracing::error!("wasapi loopback capture failed: {e:#}");
                    }
                })
                .context("spawn wasapi thread")?,
        );
        Ok(())
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for WasapiLoopCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn capture_loop(
    device_name: String,
    format: AudioFormat,
    out: Sender<PcmChunk>,
    stop: &AtomicBool,
) -> Result<()> {
    if !wasapi::initialize_mta().is_ok() {
        bail!("COM (MTA) initialization failed");
    }

    let enumerator = DeviceEnumerator::new().map_err(err)?;
    let device = if device_name.is_empty() {
        enumerator
            .get_default_device(Direction::Render)
            .map_err(err)?
    } else {
        let collection = enumerator
            .get_device_collection(Direction::Render)
            .map_err(err)?;
        let needle = device_name.to_lowercase();
        let count = collection.get_nbr_devices().map_err(err)?;
        let mut found = None;
        for i in 0..count {
            let d = collection.get_device_at_index(i).map_err(err)?;
            if d.get_friendlyname()
                .map_err(err)?
                .to_lowercase()
                .contains(&needle)
            {
                found = Some(d);
                break;
            }
        }
        found.context("endpoint disappeared before start")?
    };

    let mut audio_client = device.get_iaudioclient().map_err(err)?;
    let mixfmt: WaveFormat = audio_client.get_mixformat().map_err(err)?;
    let blockalign = mixfmt.get_blockalign() as usize;
    let (def_time, _min_time) = audio_client.get_device_period().map_err(err)?;

    // Initialize the render device's client in *capture* direction —
    // wasapi-rs sets the AUDCLNT_STREAMFLAGS_LOOPBACK flag for us.
    audio_client
        .initialize_client(
            &mixfmt,
            Direction::Capture,
            StreamMode::PollingShared {
                autoconvert: false,
                buffer_duration_hns: def_time,
            },
            None,
        )
        .map_err(err)?;

    let capture_client = audio_client.get_audiocaptureclient().map_err(err)?;
    audio_client.start_stream().map_err(err)?;
    tracing::info!(
        "wasapi loopback started on '{}' ({} ch @ {} Hz)",
        device_name,
        format.channels,
        format.sample_rate
    );

    let mut deque: VecDeque<u8> = VecDeque::with_capacity(blockalign * 8192);
    while !stop.load(Ordering::Relaxed) {
        // Polling read: returns whatever packets are buffered.
        if let Err(e) = capture_client.read_from_device_to_deque(&mut deque) {
            // Device hot-plug / format change: surface and stop.
            tracing::error!("wasapi read failed: {e}");
            let _ = audio_client.stop_stream();
            bail!("wasapi read failed: {e}");
        }
        if deque.len() >= blockalign {
            // Drain whole frames only.
            let whole = deque.len() - (deque.len() % blockalign);
            let mut bytes = Vec::with_capacity(whole);
            for _ in 0..whole {
                bytes.push(deque.pop_front().expect("len checked"));
            }
            // Mix format is f32 interleaved little endian.
            let n = bytes.len() / 4;
            let mut samples = vec![0f32; n];
            for (i, s) in samples.iter_mut().enumerate() {
                let off = i * 4;
                *s = f32::from_le_bytes([
                    bytes[off],
                    bytes[off + 1],
                    bytes[off + 2],
                    bytes[off + 3],
                ]);
            }
            let payload = f32_samples_to_bytes(&samples, format.encoding);
            if out.send(payload).is_err() {
                break;
            }
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let _ = audio_client.stop_stream();
    tracing::info!("wasapi loopback stopped");
    Ok(())
}
