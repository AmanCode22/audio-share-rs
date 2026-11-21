//! ash-core — engine library for Audio Share Rust.
//!
//! Re-creation of [mkckr0/audio-share](https://github.com/mkckr0/audio-share)
//! in Rust. Shares a computer's system audio over the LAN: the host (laptop /
//! desktop) captures its output in loopback and streams PCM to any number of
//! players (Android phone, another laptop running this crate, ...).
//!
//! Layout:
//! - [`protocol`]   — the `ASR2` wire protocol (TCP control + UDP data)
//! - [`capture`]    — audio sources: WASAPI loopback, cpal input, sine generator
//! - [`server`]     — LAN streaming server (multi-client, TCP + UDP)
//! - [`client`]     — streaming client (jitter buffer, resampling, playback)
//! - [`net`]        — LAN address helpers
//!
//! The library is dependency-injected friendly: the CLI, the Tauri desktop
//! companion and integration tests all build on the same primitives.

pub mod capture;
pub mod client;
pub mod net;
pub mod protocol;
pub mod server;

pub use protocol::{AudioFormat, Encoding};
