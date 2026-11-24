# Mobile & Cross-Platform Targets (Windows, macOS, Linux, iOS, Android, Console)

This workspace builds clean-room binaries and WebAssembly bundles for every major platform:

## 1. Desktop (Windows, Linux, macOS)
- **Windows x64**: `.\build.ps1` (produces signed `HitBoy's Emulations.exe`)
- **Linux x86_64**: `cargo build --release -p hitboy` (requires `libasound2-dev`, `libudev-dev`, `libgtk-3-dev`)
- **macOS (Apple Silicon & Intel)**: `cargo build --release -p hitboy --target aarch64-apple-darwin` or `--target x86_64-apple-darwin`
- Automated CI builds for all desktop targets are configured in `.github/workflows/build-all-platforms.yml`.

## 2. iOS, Android & Console Browsers (Installable WebAssembly PWA)
Our pure-Rust & C++ cores compile to `wasm32-unknown-unknown` via `crates/web` and run full-screen with:
- **On-screen touch controller** for iPhone, iPad, and Android phones/tablets
- **Native Gamepad API** for Xbox Edge, PlayStation, Steam Deck, Backbone, DualSense, and Xbox Wireless Controllers
- **Home-screen installation** via `manifest.json` (`display: standalone`) on iOS Safari ("Add to Home Screen") and Android Chrome ("Install App").

## 3. Native Android (APK / NDK) & Native iOS (XCFramework)
- **Android NDK**:
  ```bash
  rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
  cargo ndk -t arm64-v8a build --release -p frontend
  ```
- **iOS Static Library**:
  ```bash
  rustup target add aarch64-apple-ios
  cargo build --release --target aarch64-apple-ios -p psp -p nds -p n3ds -p dreamcast -p neogeo -p nes -p snes -p gba -p gb -p md
  ```
