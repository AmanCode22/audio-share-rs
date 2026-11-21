//! Audio sources that feed the streaming server.
//!
//! All capturers produce **interleaved PCM bytes in the negotiated target
//! encoding** (see [`protocol::Encoding`]) through a `std::sync::mpsc`
//! channel — audio capture lives on OS threads, not async tasks.

pub mod convert;
pub mod sine;

#[cfg(feature = "audio")]
pub mod cpal_in;
#[cfg(all(feature = "audio", target_os = "windows"))]
pub mod wasapi_loop;

use std::sync::mpsc::Sender;

use anyhow::Result;

use crate::protocol::AudioFormat;

/// Bytes of PCM produced by a capturer.
pub type PcmChunk = Vec<u8>;

/// A running or startable audio source.
pub trait Capturer: Send + 'static {
    /// Format of the PCM this capturer will emit (post-conversion).
    fn format(&self) -> AudioFormat;

    /// Begin capturing; chunks are sent until [`Capturer::stop`] or the
    /// receiver is dropped. Must not block.
    fn start(&mut self, out: Sender<PcmChunk>) -> Result<()>;

    /// Stop capturing and release the device.
    fn stop(&mut self);
}

/// Which source feeds the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSelection {
    /// System output loopback (what viewers hear). Windows: WASAPI loopback
    /// of the chosen render endpoint. Linux/macOS: falls back to an input
    /// device (use a monitor source / BlackHole for true loopback).
    Loopback { device: Option<String> },
    /// A physical input (microphone) or monitor source by name.
    Input { device: Option<String> },
    /// Deterministic sine generator — CI, testing, demos. No hardware needed.
    Sine { sample_rate: u32, channels: u16 },
}

/// A capture device available on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub is_default: bool,
}

/// Input devices available to cpal (microphones, monitor sources).
#[cfg(feature = "audio")]
pub fn list_input_devices() -> Result<Vec<DeviceInfo>> {
    cpal_in::list_devices()
}

/// Render endpoints usable as loopback sources. Only Windows can capture
/// arbitrary output endpoints natively; elsewhere the list is empty and
/// users pick a monitor/virtual device from [`list_input_devices`].
#[cfg(all(feature = "audio", target_os = "windows"))]
pub fn list_loopback_devices() -> Result<Vec<DeviceInfo>> {
    wasapi_loop::list_render_endpoints()
}

#[cfg(all(feature = "audio", not(target_os = "windows")))]
pub fn list_loopback_devices() -> Result<Vec<DeviceInfo>> {
    Ok(Vec::new())
}

/// Instantiate a capturer for `selection`, converting PCM to
/// `format.encoding` at the device's native rate/channels.
pub fn open_capturer(
    selection: &SourceSelection,
    encoding: crate::protocol::Encoding,
) -> Result<Box<dyn Capturer>> {
    match selection {
        SourceSelection::Sine {
            sample_rate,
            channels,
        } => Ok(Box::new(sine::SineCapturer::new(
            *sample_rate,
            *channels,
            encoding,
        ))),
        #[cfg(feature = "audio")]
        SourceSelection::Loopback { device } => {
            #[cfg(target_os = "windows")]
            {
                return Ok(Box::new(wasapi_loop::WasapiLoopCapturer::new(
                    device.clone(),
                    encoding,
                )?));
            }
            #[cfg(not(target_os = "windows"))]
            {
                Ok(Box::new(cpal_in::CpalCapturer::input(
                    device.clone(),
                    encoding,
                )?))
            }
        }
        #[cfg(feature = "audio")]
        SourceSelection::Input { device } => Ok(Box::new(cpal_in::CpalCapturer::input(
            device.clone(),
            encoding,
        )?)),
        #[cfg(not(feature = "audio"))]
        _ => anyhow::bail!("audio feature disabled: only the sine source is available"),
    }
}
