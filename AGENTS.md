# AGENTS.md — AI Coding Agent Handbook

Welcome! This repository is **HitBoy's Emulations**, a 100% clean-room Rust + C++ multi-console emulator, virtual machine suite, WebAssembly browser player, and Xbox/PS5 native executable converter.

This document is written specifically for AI coding agents (Claude, Gemini, Codex, Cursor, etc.) and human engineers picking up work in this codebase.

---

## 1. Strict Legal, Licensing & Clean-Room Rules

> **NON-NEGOTIABLE RULES — READ BEFORE WRITING ANY CODE**

1. **100% Clean-Room Implementation Only**:
   - **NEVER** copy code from GPL, LGPL, AGPL, or unlicensed third-party repositories (including `TomBox`, `xbox-emulator`, `AnyPS5`, `shadPS4`, `RPCS3`, `Xenia`, `Dolphin`, `PPSSPP`, `Citra`, `PCSX2`, etc.).
   - You may study public hardware manuals, AMD ISA PDFs, Microsoft DirectX / GDK public headers, Khronos specs, and high-level architectural concepts, then write every line of code independently from scratch.
   - Document any new public reference consulted in [SOURCES.txt](SOURCES.txt).
2. **No Copyrighted Game Files, Keys, or Piracy**:
   - **NEVER** fetch, download, commit, decrypt, or inspect copyrighted retail ROMs, ISOs, console encryption keys, BIOS dumps, or leaked game builds.
   - Always test using synthetic in-memory binaries, clean-room unit tests, or openly licensed homebrew.
3. **Zero Personal Machine Paths**:
   - Never hardcode local user paths (e.g., `C:\Users\<name>\...`) in tests, configs, or scripts. Always use `std::env::temp_dir()` or relative workspace paths.
4. **Zero Compiler Warnings**:
   - Every change must compile cleanly with **0 warnings** on both native desktop (`cargo test --workspace --exclude web`) and WebAssembly (`cargo build -p web --release --target wasm32-unknown-unknown`).

---

## 2. Brand Theme & Visual Identity

- **HitBoy's Emulations** (Open Source):
  - Primary palette: **Black** (`#000000`), **Royal Blue** (`#4169E1`), **Royal Purple** (`#7851A9`).
  - Clean game titles everywhere: strip dump tags like `(World)`, `(USA)`, `(Rev A)`, `(En,Fr,De)`, `[!]` before displaying in both the desktop UI and WebAssembly player.

---

## 3. Repository Layout (Key Crates)

| Area | Crates | Purpose |
| :--- | :--- | :--- |
| **Desktop Frontend & Apps** | `crates/frontend`, `crates/apps/hitboy` | `eframe`/`egui` desktop app, Home Hero Launchpad, Console Mode, ROM scanner, cover art, save states, rewind, VM runner, Xbox/PS5 converter UI |
| **Web / Mobile / Console PWA** | `crates/web`, `websites/` | `wasm-bindgen` browser player (`play.html`), touch overlays, PWA `manifest.json`, iOS guide (`ios.html`) |
| **Xbox One / Series X\|S** | `crates/xboxone`, `crates/x2w` | PE64 loader, sandboxed x86-64 core, Era 4K/64K/2M memory manager, DirectStorage COM (`dstorage.rs`), `ID3D12DeviceX` + `dxcompiler.dll` bridge (`d3d12x_com.rs`), ACP/APU audio (`apu_audio.rs`), RDNA2 disassembler + HLSL translator (`rdna2_disasm.rs`), static Xbox→Windows PE converter (`exe_convert.rs`), and standalone companion runtime DLL **`x2w.dll`** (`crates/x2w`) |
| **PlayStation 5 / PS4** | `crates/ps5` | ELF64 loader, NID HLE kernel, PS5/PS4 ELF → Windows x64 PE32+ converter with `fs:` TLS/stack-canary relinker (`exe_convert.rs`), `SLB2`/`PS5UPDATE.PUP` firmware inspector (`firmware.rs`) |
| **Original Xbox & Xbox 360** | `crates/xbox`, `crates/x360`, `crates/cpuxenon` | Original Xbox x86 core and Xbox 360 PowerPC64 (`cpuxenon`) core |
| **Sony PlayStation Family** | `crates/psx`, `crates/ps2`, `crates/ps3`, `crates/psp`, `crates/vita` | PS1 (R3000), PS2 (R5900), PS3 (Cell PPU on `cpuxenon`), PSP (Allegrex MIPS32R2 + GE 3D rasterizer), PS Vita (ARMv7 OLED) |
| **Nintendo Consoles & Handhelds** | `crates/nes`, `crates/snes`, `crates/n64`, `crates/gc`, `crates/wii`, `crates/wiiu`, `crates/gb`, `crates/gba`, `crates/nds`, `crates/n3ds`, `crates/switch` | Complete Nintendo lineage using clean-room CPU cores (`cpu6502`, `cpu65816`, `cpuvr4300`, `cpugekko`, `cpusm83`, `cpuarm7`, `cpuarm_v7`) |
| **Sega / Arcade / Retro** | `crates/sms`, `crates/md`, `crates/saturn`, `crates/dreamcast`, `crates/neogeo`, `crates/pce`, `crates/ws`, `crates/vb`, `crates/atari2600`, `crates/atari7800`, `crates/intv` | Master System, Genesis/MD, Saturn (SH-2), Dreamcast (SH-4 + PowerVR2 + Maple Bus), Neo Geo (M68K), PC Engine, WonderSwan, Virtual Boy, Atari, Intellivision |
| **VMs & Mobile** | `crates/pc`, `crates/pchw`, `crates/android`, `crates/ios` | Full x86 PC VM (VESA HD/Tablet/Portrait modes), Android 14 APK parser + Dalvik VM + interactive launcher, iOS ARM64 runner |
| **Utilities** | `crates/state`, `crates/state_derive`, `crates/gamedump` | Save-state serialization macros and user disc/console dump verification helper |

---

## 4. Essential Commands

```powershell
# Run targeted crate tests (fast)
cargo test -p xboxone -p x2w -p ps5 -p frontend

# Run full workspace tests (excluding wasm-only web crate)
cargo test --workspace --exclude web

# Verify WebAssembly build (requires wasm32-unknown-unknown target + LLVM clang on PATH)
cargo build -p web --release --target wasm32-unknown-unknown

# Build full release desktop apps, x2w.dll, WASM player, and portable zips (Windows)
.\build.ps1
```

---

## 5. Known Gotchas & Solved Traps

1. **`crates/web` depends on `crates/xboxone`**:
   - Anything added to `crates/xboxone` MUST compile cleanly on **both** `x86_64-pc-windows-msvc` AND `wasm32-unknown-unknown`!
   - Guard all Windows FFI (`#[link(name = "kernel32")]`, `LoadLibraryA`, `VirtualAlloc`, etc.) with `#[cfg(all(windows, target_arch = "x86_64"))]` and provide clean fallback stubs under `#[cfg(not(all(windows, target_arch = "x86_64")))]`.
2. **In-Place PE Import Rewriting (`x2w.dll`)**:
   - Static PE rewriting patches DLL names in-place inside `.rdata`/`.idata`, so the target DLL name must satisfy `target.len() <= original.len()`.
   - Because `x2w.dll` is **7 bytes**, it fits cleanly in-place over EVERY Xbox console DLL (`xmem.dll` = 8, `xg_x.dll` = 8, `d3d12_xs.dll` = 12, `dstorage_xs.dll` = 15, `xgameruntime.dll` = 16).
3. **`crates/cpugekko/cpp/gekko.cpp` Timebase**:
   - Always keep `c->tb_mul = 1;` in `gekko_reset()` (setting it to `0` causes `STATUS_INTEGER_DIVIDE_BY_ZERO` on Wii / Wii U idle loops).
4. **Archive Dispatch in `crates/frontend/src/cores.rs`**:
   - When adding a new console core, wire it into `next_wave::from_bytes_for` AND `library.rs` AND `archive::first_game` so zipped ROMs dispatch to the right core instead of falling through.

---

## 6. Further Documentation

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — System-wide architecture, frontend lifecycle, core traits, and WASM pipeline.
- [docs/XBOX_AND_PS5_INTERNALS.md](docs/XBOX_AND_PS5_INTERNALS.md) — Deep dive into Xbox Series X|S emulation, `x2w.dll`, D3D12.XS/DXC, DirectStorage, APU audio, and PS5 ELF→PE conversion.
- [docs/CONTRIBUTING_AI.md](docs/CONTRIBUTING_AI.md) — Step-by-step recipes for AI agents adding shims, GPU translation passes, and console cores.
- [SOURCES.txt](SOURCES.txt) — Public specifications and architectural references.
