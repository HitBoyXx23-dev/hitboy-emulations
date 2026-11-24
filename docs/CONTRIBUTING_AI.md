# AI Contributor Playbook

Use these concrete checklists whenever you extend this repository.

---

## Recipe 1: Adding a New Xbox Series X|S API Shim (Both Emulator & `x2w.dll`)

1. **Implement the clean-room function** in `crates/xboxone/src/shims.rs` inside `mod win` as `pub extern "system" fn MyXboxApi(...) -> i32` (never panic across the FFI boundary!).
2. **Register it in `shims::resolve()`** (`crates/xboxone/src/shims.rs`) using `eq_ignore_ascii_case`.
3. **Add it to `crates/xboxone/src/x64_emu.rs`** if sandboxed Emulator Mode should also dispatch it via HLE.
4. **Export it from `x2w.dll`** in `crates/x2w/src/lib.rs` using `#[no_mangle] pub extern "system" fn MyXboxApi(...)` delegating to `xboxone::shims::MyXboxApi`.
5. **Register any new console DLL name** in:
   - `crates/xboxone/src/modules.rs` (`Provider::Shim`)
   - `crates/xboxone/src/exe_convert.rs` (`mapped_windows_dll` → `Some("x2w.dll")`)
   - `crates/xboxone/src/native.rs` (`resolve_console_dll`)
6. **Add a unit test** covering the new API in `crates/xboxone` and `crates/x2w`.
7. **Verify both native and WASM builds**:
   ```powershell
   cargo test -p xboxone -p x2w -p frontend
   cargo build -p web --release --target wasm32-unknown-unknown
   ```

---

## Recipe 2: Adding a New Console Core

1. Create `crates/<console>/Cargo.toml` and `crates/<console>/src/lib.rs` (pure clean-room Rust or C++ via `cc`), and add `"crates/<console>"` to root `Cargo.toml`.
2. Implement load, step-frame, RGBA8 framebuffer, PCM audio, input bitmask, and reset.
3. Add a unit test executing synthetic bytecode and verifying non-zero framebuffer or CPU state progression.
4. Wire it into `crates/frontend/Cargo.toml`, `crates/frontend/src/library.rs`, `crates/frontend/src/cores/next_wave.rs`, and `crates/frontend/src/cores.rs` (`archive::first_game`).
5. Optionally expose it in `crates/web/src/lib.rs` and `websites/play.html` for browser/mobile PWA play.

---

## Recipe 3: Pre-Push Quality Gate

Before committing or shipping a build, verify all four checks pass:

- [ ] `cargo test --workspace --exclude web` passes with **0 failures** and **0 warnings**.
- [ ] `cargo build -p web --release --target wasm32-unknown-unknown` succeeds with **0 warnings**.
- [ ] Zero hardcoded user paths (`C:\Users\...`) anywhere in tracked files.
- [ ] Zero third-party GPL / unlicensed code copied; public specs documented in `SOURCES.txt`.
