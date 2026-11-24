# Emulation contributor handoff

## Custom cores

Use `crates/xbox` for original Xbox BIOS and MCPX boot work, `crates/xboxone` for the Xbox One and Series X|S interpreter and native PE loader, and `crates/x2w` for the Xbox-to-Windows companion DLL. Use `crates/ps5` for the PS5 core and Windows executable conversion. `crates/gamedump` provides command-line diagnostics. `SOURCES.txt` lists research references.

## Verification

- Run `cargo test -p xbox -p xboxone -p x2w -p ps5 -p gamedump` for the console code. Use source-built fixtures for development and regression tests.
- Use `gamedump xbox --inspect-only <game-folder>` to list native import coverage before executing a title. Use `gamedump xbox-emu <game-folder> <steps>` to stop the custom interpreter at the first missing instruction or API within an explicit budget.
- For a native launch, record the executable build, startup output, first verified game frame, and whether controllers or keyboard input actually reaches the title. Record Xbox One, Series S, and Series X separately.
- The original Xbox core has a flash program boot test. For a real firmware boot, record the firmware version and evidence of executed code and video. Xbox One, Series X|S, and PS5 firmware inspectors classify inputs. A successful inspection alone does not establish firmware boot.
- Keep game files, console firmware, and keys outside the repository. Tests and public reports should contain only source-built fixtures and diagnostic results.

## Host targets

- Windows x64 is the first target for Xbox and PS5 native translation. Verify Linux and macOS backends independently before declaring support.
- Browser builds and native iOS or Android builds require their own compatibility checks. On ARM64, x64 console executables need interpretation or CPU translation.
- Console-hosted builds are exploratory and depend on permitted deployment and host capabilities.

## Branding

The shared desktop theme lives in `crates/frontend/src/theme.rs`. HitBoy uses a black base, royal blue primary, and royal purple secondary. Navuryx uses the same black base with royal purple primary and royal blue secondary. Test both launchers after changing shared colors, icons, or input UI.
