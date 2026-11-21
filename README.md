# Audio Share Rust

**Share your computer's audio over the LAN** — your phone (or another
laptop) becomes the wireless speaker of your PC. Everything host-side is
pure Rust: a library, a CLI and a Tauri 2 desktop companion. The Android
player is a Jetpack Compose app with Material 3 Expressive.

> A from-scratch re-creation of [mkckr0/audio-share](https://github.com/mkckr0/audio-share)
> with better everything: modern Rust engine, self-describing binary
> protocol, mDNS discovery, TCP-only fallback for UDP-blocked networks,
> cross-platform GUI, fully YAML-driven builds — desktop *and* Android —
> and an end-to-end tested core. See the original project for the prior
> art; this codebase shares no code with it.

---

## Feature comparison vs. the original

| | original | this project |
|---|---|---|
| Host engine | C++20 (ASIO/protobuf) | Rust + tokio (`ash-core`) |
| Host GUI | MFC, Windows-only | Tauri 2 (GTK/WebKitGTK on Linux, WebView2 on Windows, WebKit on macOS) |
| Host CLI | C++ `as-cmd` | Rust `ashr` |
| Capture | WASAPI / PipeWire | WASAPI loopback / cpal (Pulse monitor, BlackHole) / sine test source |
| Wire protocol | protobuf + raw cmd ints | self-describing `ASR2` frames (see [docs/PROTOCOL.md](docs/PROTOCOL.md)) |
| Discovery | manual IP entry | mDNS (`_audiosharers._tcp`) + NSD on Android + desktop scan |
| UDP-blocked networks | unsupported | `--tcp-data` mode streams audio over TCP |
| Client metrics | none | buffered ms, packet loss, RTT (server- and client-side) |
| Multi-client | yes | yes, with per-client stats + timeouts |
| Builds | MSVC/CMake + Gradle | GitHub Actions YAML: 3 desktop OS + Android APK |
| Tests | unit only | protocol unit tests + full server↔client loopback integration tests |

## Components

```
audio-share-rs/
├── crates/ash-core      engine library: protocol, capture, server, client, sinks
├── crates/ash-cli       `ashr` — serve / play / devices / addresses
├── apps/desktop         Tauri 2 companion (GTK-styled UI, host + player, mDNS scan)
├── android/             Kotlin Compose client (Material 3 Expressive, NSD, QS tile)
├── .github/workflows/   ci.yml · release.yml (+ dependabot)
└── docs/PROTOCOL.md     byte-level wire spec
```

## Quick start

### Laptop — share audio

```sh
# CLI
ashr serve                       # bind LAN, default port 65530, WASAPI loopback
ashr serve --source sine         # demo stream without audio hardware
ashr serve --device "Speakers" --encoding f32 --block-ms 20

# GUI
cargo tauri dev  # from apps/desktop — or install the release bundle
```

The server prints the address players should use (e.g. `192.168.1.10:65530`),
advertises itself via mDNS, and shows live per-client stats.

### Phone — play

Install the APK (built by CI, or `cd android && gradle assembleRelease`) and:

1. tap **Scan** to find servers on the network, or type the laptop's IP,
2. pick your buffer/volume, press **Play**.

A Quick Settings tile starts/stops playback with the last-used settings.

### Another laptop — play

```sh
ashr play --host 192.168.1.10 --buffer-ms 120
ashr play --host 192.168.1.10 --tcp-data   # when UDP is blocked
```

## Building from source

Requirements: Rust 1.77+ (`rustup`), and for the desktop app a Tauri
environment (`cargo install tauri-cli`, plus `libwebkit2gtk-4.1-dev
libgtk-3-dev libayatana-appindicator3-dev` on Linux). Android needs JDK 17
and the Android SDK.

```sh
cargo test --workspace                 # unit + loopback integration tests
cargo build --release -p ash-cli       # CLI binary
cargo tauri build                      # desktop bundles (deb/AppImage/dmg/msi/nsis)
cd android && gradle assembleRelease    # Android release APK (debug-signed without secrets)
python3 scripts/gen_icons.py                      # source app-icon.png
cargo tauri icon apps/desktop/app-icon.png        # .ico/.icns/.png set for bundlers
```

## CI/CD (YAML builds)

| workflow | trigger | what it does |
|---|---|---|
| `ci.yml` | push / PR | fmt + clippy + release-profile tests + release build on Ubuntu/Windows/macOS, desktop smoke compile, release APK |
| `release.yml` | tag `v*` push (or manual) | CLI archives, Tauri bundles (msi/nsis, deb/rpm/AppImage, dmg on macOS ARM64 + Intel), release APK + AAB, SHA256 sums, GitHub Release |
| dependabot | weekly | cargo + gradle + actions bumps |

Releasing: `git tag v1.0.0 && git push --tags`. Android signing uses
repository secrets `ANDROID_KEYSTORE_BASE64` (+ `ANDROID_KEYSTORE_PASSWORD`,
`ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD`); without them the release APK/AAB
is signed with the auto-generated debug key so it stays installable.

## Firewall notes

**Windows** prompts to add the rule on first run. **Linux** (firewalld):

```sh
port=65530
sudo firewall-cmd --add-rich-rule="rule family=ipv4 port port=$port protocol=tcp accept"
sudo firewall-cmd --add-rich-rule="rule family=ipv4 port port=$port protocol=udp accept"
sudo firewall-cmd --runtime-to-permanent
```

## No audio endpoint? (headless / dummy sinks)

**Linux / PipeWire** — create a virtual sink (adapted from the original
project's recipe), then select it as the loopback source:

```sh
mkdir -p ~/.config/pipewire/pipewire.conf.d
cp scripts/audio-share-sink.conf ~/.config/pipewire/pipewire.conf.d/
systemctl --user restart pipewire
ashr serve --device "Audio Share Sink"
```

**macOS** — install [BlackHole](https://existential.audio) and set it as
output + capture device: `ashr serve --device "BlackHole"`.

## License

Apache-2.0 (see [LICENSE](LICENSE) and [NOTICE](NOTICE)). The original
project by mkckr0 is Apache-2.0 as well; this re-implementation credits it
and documents protocol incompatibility in the NOTICE file.
