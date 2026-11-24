# Xbox Series X|S & PlayStation 5 Internals

This technical guide details our clean-room Xbox One / Series X|S (`crates/xboxone`, `crates/x2w`) and PlayStation 5 (`crates/ps5`) subsystems so other AI agents can continue advancing compatibility systematically.

---

## 1. Xbox One / Series X|S Architecture (`crates/xboxone`)

Our runtime supports **three execution/conversion modes** from the same clean-room core:

1. **Emulator Mode (`x64_emu.rs`)**:
   - Sandboxed x86-64 interpreter with `GS:` TEB/PEB, static TLS callbacks, cooperative threading, BMI2/SSE2 ops, Era virtual memory (`era_mem.rs`: 4 KiB / 64 KiB / 2 MiB pages + physical page aliasing), and HLE import dispatch.
2. **In-Process Native Mode (`native.rs` + `shims.rs`)**:
   - Maps an unencrypted x86-64 PE directly into host Windows memory with real SEH unwind tables, `VirtualAlloc2` placeholder/physical-page aliasing (`native_pages.rs`), dynamic `LoadLibraryA/W` / `GetProcAddress` console-DLL redirection, and direct native execution of clean-room Xbox API shims.
3. **Static Xbox → Windows Native `.exe` Mode (`exe_convert.rs` + `crates/x2w`)**:
   - Patches PE32+ `Subsystem` from `14` (Xbox) → `2` (Windows GUI), clears the PE checksum, and rewrites Xbox console-only DLL names in the Import Directory in-place to **`x2w.dll`** (or standard Windows DLLs like `kernel32.dll` / `advapi32.dll`).
   - Ships a real compiled Windows x64 companion DLL (**`x2w.dll`**) that exports all clean-room Xbox APIs so the Windows OS loader itself can launch `<game>_windows.exe` directly.

### Key Modules in `crates/xboxone/src/`

| File | Responsibility |
| :--- | :--- |
| `pe.rs` & `package.rs` | PE32+ parser, import/export/section inspection, `MicrosoftGame.config` (GDK) and `AppxManifest.xml` (XDK) manifest reader, encrypted XVD/XVC detection (`msft-xvd` at offset `0x200`). |
| `era_mem.rs` & `native_pages.rs` | Xbox unified memory model (Series X 13.5 GiB title RAM budget, `XMemVirtualAlloc` 64K/2M page modes, `XMemAllocatePhysicalPages`, `MapViewOfFile3` physical page aliasing). |
| `dstorage.rs` | Real Windows x64 COM vtables for `IDStorageFactoryX`, `IDStorageFileX`, and `IDStorageQueueX` streaming real byte ranges from disk into guest memory on `Submit()`. |
| `d3d12x_com.rs` | Real Windows x64 COM vtables for `ID3D12DeviceX` and `ID3D12GraphicsCommandListX`, desktop `d3d12.dll` device probe, nested occlusion-query un-nesting, frame pacing (`SetFrameIntervalX` / `ScheduleFrameEventX`), `DispatchMesh`, and `compile_hlsl_to_dxil()` calling `dxcompiler.dll!DxcCreateInstance(CLSID_DxcCompiler, IID_IDxcCompiler3)`. |
| `rdna2_disasm.rs` & `gpu_bridge.rs` | RDNA2 (GFX10.3) opcode decoder, HLSL translator with Mesh/Amplification stage detection, automatic `SynthesizedRootSignature`, 64-bit FNV-1a `ShaderCache`, DXBC console chunk (`XDAT`/`XBOX`/`SCAR`) stripping, root-signature high-flag sanitization, and XG BC1–BC7/HDR texture mip layout calculator. |
| `apu_audio.rs` | Real COM vtable for `IAcpHal` (`Connect`, `Disconnect`, `SubmitCommand`), SHAPE context allocator, and PCM16/PCM32F/XMA2/Opus stereo mixer. |
| `shims.rs` & `modules.rs` | 80+ Xbox GDK/XDK/Series X|S API shims (`XGameRuntime*`, `XUser*`, `XTaskQueue*`, `XAsync*`, `XGameSave*` with disk persistence, `XMem*`, `XNetworking*`, `XStore*`, `DStorage*`, `D3D12XBOX_CreateDevice`, `AcpHal*`, `ApuAlloc/Free`, `Xcurl*`, `Bink*`). |
| `exe_convert.rs` | Static PE converter, per-DLL/per-function `CoverageReport`, `OcclusionQueryUnnester`, `FramePacer`, and `OfflineServices`. |
| `firmware.rs` | Clean-room inspector for Xbox XVD, Xbox 360 NAND, and Original Xbox BIOS images. |

### `x2w.dll` Import Remapping Table

| Xbox Console DLL | Rewritten Target | Reason |
| :--- | :--- | :--- |
| `d3d12_xs.dll`, `d3d12_x.dll` | `x2w.dll` | Wraps `D3D12XBOX_CreateDevice`, `SetFrameIntervalX`, `D3D12SerializeRootSignature`, and forwards standard D3D12 calls to Windows `d3d12.dll`. |
| `dstorage_xs.dll`, `dstorage.dll` | `x2w.dll` | Routes to our real file-streaming `IDStorageFactoryX` COM engine. |
| `xg_xs.dll`, `xg_x.dll` | `x2w.dll` | Routes `XGComputeOptimalTileMode` and texture layout helpers. |
| `xgameruntime.dll`, `xgameplatform.dll` | `x2w.dll` | Routes GDK/XDK runtime, user, task queue, save, and offline service APIs. |
| `xmem.dll` | `x2w.dll` | Routes `XMemVirtualAlloc`, `XMemAlloc*`, `TitleMemoryStatus`. |
| `acphal.dll`, `xapu.dll`, `chad.dll` | `x2w.dll` | Routes `AcpHalCreate`, `ApuAlloc/Free`, `XAudio2Create`. |
| `xcurl.dll` | `x2w.dll` | Offline HTTP shim. |
| `kernelx.dll` | `kernel32.dll` | Standard Win32 kernel exports. |
| `d3d11_x.dll` | `d3d11.dll` | Standard desktop D3D11. |
| `xcrdapi.dll` | `advapi32.dll` | Standard Windows credentials/security API. |

---

## 2. PlayStation 5 / PS4 Architecture (`crates/ps5`)

- **ELF64 & NID Runtime**: Loads decrypted PS5/PS4 `eboot.bin` / ELF64 images, resolves Orbis/Prospero library NIDs to clean-room HLE kernel/video/audio/pad shims.
- **Static ELF → Windows PE32+ Converter (`crates/ps5/src/exe_convert.rs`)**:
  - Translates loadable ELF `PT_LOAD` segments into a valid Windows x64 PE32+ `.exe`.
  - Scans executable code for FreeBSD/PlayStation `fs:` (`0x64` prefix) TLS / stack-canary accesses (`fs:[0x28]`) and relinks them for Windows x64 (`gs:` / safe stack-canary slot).
- **Firmware Inspector (`crates/ps5/src/firmware.rs`)**: Parses `SLB2` and `PS5UPDATE.PUP` container headers and reports entry tables cleanly.

---

## 3. Next High-Impact Engineering Milestones (Roadmap for Continuing AIs)

To move modern AAA Xbox Series X|S and PS5 titles from startup/init toward live 3D gameplay, tackle these in order:

1. **Full Desktop `ID3D12Device` + `ID3D12GraphicsCommandList` Slot Forwarding (`d3d12x_com.rs`)**:
   - Expand `D3D12DeviceXVtable` to match the full 44-slot `ID3D12Device` COM vtable layout + Xbox extension slots, storing an inner real `*mut c_void` PC `ID3D12Device` handle created via `d3d12.dll!D3D12CreateDevice` and forwarding resource, heap, descriptor, PSO, command-queue, and swapchain creation directly to the PC GPU.
2. **Real RDNA2 → HLSL → DXIL Pipeline Cache (`rdna2_disasm.rs` + `d3d12x_com.rs`)**:
   - Expand `rdna2_disasm.rs` to decode full GFX10.3 SOP/VOP/SMEM/MUBUF/MIMG/EXPORT instructions into valid Shader Model 6.5+ HLSL, compile through `compile_hlsl_to_dxil()` (bundle or locate `dxcompiler.dll`), and cache compiled DXIL blobs on disk by 64-bit FNV-1a hash.
3. **XG Swizzle / Deswizzle Kernels (`gpu_bridge.rs`)**:
   - Implement AMD RDNA2 64KB_S / 64KB_D micro-tile swizzle & deswizzle math for `BC1`–`BC7` and render targets so console-tiled textures upload properly to PC D3D12.
4. **Live XMA2 / Opus Hardware Audio Decoding (`apu_audio.rs`)**:
   - Replace the clean-room frame envelope placeholder with full clean-room XMA2 bitstream & Opus packet decoding fed into desktop `xaudio2_9.dll` mastering/source voices.
5. **PS5 Vulkan / DX12 AGC Command-Buffer Translator (`crates/ps5`)**:
   - Parse PM4 Type-3 GPU command packets (`DRAW_INDEX_2`, `DISPATCH_DIRECT`, `SET_SH_REG`) and translate them into desktop DX12 / Vulkan draw calls.
