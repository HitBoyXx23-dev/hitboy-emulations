//! The Xbox One / Series X|S system libraries, and what each one is on a PC.
//!
//! An Xbox executable imports from the same idea of libraries Windows has, but with Xbox's own
//! names: `kernelx.dll` instead of `kernel32.dll`, `d3d12_x.dll` instead of `d3d12.dll`. Most
//! functions are the Windows ones under a different file name, so native mode can satisfy them by
//! resolving the same name out of the real Windows DLL. The rest are Xbox-only (the game runtime,
//! storage, the title's own saved data) and need a replacement we write ourselves.

/// How one Xbox library is satisfied on a PC.
#[derive(Debug, Clone, PartialEq)]
pub enum Provider {
    /// The same function name exists in this Windows DLL.
    Windows(&'static str),
    /// Ours to implement: `dll!function` and how hard it is.
    Shim {
        func: &'static str,
        need: Need,
    },
    /// Has no Windows counterpart at all; the game cannot run without a real implementation.
    Unavailable,
}

/// How much work a shim is, so the app can sort what a game is still waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Need {
    /// A string, number or memory call we can answer today.
    Easy,
    /// Needs one of our subsystems (a window, a sound buffer, a file).
    Medium,
    /// Needs the Xbox-specific subsystem behind it (saved data, networking, the title system).
    Hard,
}

impl Need {
    pub fn label(self) -> &'static str {
        match self {
            Need::Easy => "basic",
            Need::Medium => "needs a subsystem",
            Need::Hard => "needs an Xbox subsystem",
        }
    }
}

/// Every Xbox library native mode knows about, mapped to what provides it on Windows.
pub fn provider(dll: &str) -> Option<Provider> {
    let n = dll.to_ascii_lowercase();
    let hit = |f: &'static str, need: Need| -> Provider { Provider::Shim { func: f, need } };
    Some(match n.as_str() {
        // The Xbox kernel: the Windows 32-bit API surface, renamed.
        "kernelx.dll" | "kernelbase.dll" | "kernel32.dll" => Provider::Windows("kernel32.dll"),
        "ntdll.dll" => Provider::Windows("ntdll.dll"),
        "api-ms-win-core-synch-l1-2-0.dll" => Provider::Windows("kernelbase.dll"),
        // Direct3D, Xbox-flavoured. The X build adds console and shader-model features we don't
        // provide, but the D3D11/D3D12 entry points themselves are the Windows ones.
        "d3d11.dll" | "d3d11_x.dll" | "d3d11_xs.dll" | "d3d11core.dll" => Provider::Windows("d3d11.dll"),
        "d3dcompiler.dll" | "d3dcompiler_47.dll" => Provider::Windows("d3dcompiler_47.dll"),
        "dxgi.dll" => Provider::Windows("dxgi.dll"),
        "dxcore.dll" => Provider::Windows("dxcore.dll"),
        "d3d12.dll" | "d3d12_x.dll" | "d3d12_xs.dll" | "d3d12core.dll" => Provider::Windows("d3d12.dll"),
        "dstorage_xs.dll" | "dstorage.dll" => hit("DirectStorage async streaming", Need::Medium),
        "xapu.dll" | "acphal.dll" | "chad.dll" => hit("Xbox APU / DSP audio", Need::Medium),
        "xcurl.dll" => hit("Xbox HTTP / offline service", Need::Easy),
        "xcrdapi.dll" => hit("Xbox credential & profile API", Need::Easy),
        "bink2scarlett.dll" | "bink2w64.dll" => hit("Bink2 video playback", Need::Easy),
        // The C runtime is the real one; only the UCRT the game was compiled against is missing.
        // `api-ms-win-crt-*.dll` are the forwarder shims over ucrtbase, so they all land there.
        "ucrtbase.dll" | "msvcrt.dll" | "vcruntime140.dll" | "vcruntime140_1.dll" => Provider::Windows("ucrtbase.dll"),
        "ole32.dll" | "combase.dll" | "shell32.dll" | "shcore.dll" => Provider::Windows("ole32.dll"),
        "oleaut32.dll" => Provider::Windows("oleaut32.dll"),
        "ws2_32.dll" | "iphlpapi.dll" | "dnsapi.dll" | "winhttp.dll" => Provider::Windows("ws2_32.dll"),
        "userenv.dll" | "powrprof.dll" => Provider::Windows("userenv.dll"),

        // Xbox-only libraries.
        "xgameruntime.dll" => hit("XGameRuntimeInitialize", Need::Medium),
        "xgameplatform.dll" => hit("XGameRuntimeInitialize", Need::Medium),
        "xmem.dll" => hit("XMemVirtualAlloc", Need::Medium),
        "xgamefeature.lib" | "xgamefeature.dll" => hit("XGameFeatureInitialize", Need::Hard),
        "xboxone.dll" | "xboxone_x64.dll" => hit("XSystemGetXboxLiveSandboxId", Need::Hard),
        "xboxservices.lib" | "xboxservices.dll" => Provider::Unavailable,
        "xboxnet.dll" => Provider::Unavailable,
        "xstorage.dll" | "xstorage.lib" => hit("XStorageCreateHandle", Need::Hard),
        "xuser.lib" | "xuser.dll" => hit("XUserRegisterForChangeEvent", Need::Hard),
        "xg.dll" | "xg_x.dll" | "xg_xs.dll" => hit("XGComputeOptimalTileMode", Need::Medium),
        "pixevent.dll" | "pixevt.dll" => hit("XThreadIsTimeSensitive", Need::Easy),
        // Controllers and sound use the same API on Windows, so the real DLLs answer them: the
        // game then reads this PC's gamepads and plays through this PC's speakers.
        "xinput.dll" | "xinput1_4.dll" | "xinput1_3.dll" | "xinput9_1_0.dll" => Provider::Windows("xinput1_4.dll"),
        "gameinput.dll" => Provider::Windows("gameinput.dll"),
        "x3daudio.dll" | "x3daudio1_7.dll" => Provider::Windows("x3daudio1_7.dll"),
        "xaudio2.dll" | "xaudio2_9.dll" | "xaudio2_8.dll" => Provider::Windows("xaudio2_9.dll"),
        "xapofx.dll" | "xapofx1_5.dll" => Provider::Windows("xapofx1_5.dll"),
        "xact3.dll" => Provider::Unavailable,
        "libcurl.dll" => Provider::Windows("winhttp.dll"),
        "xboxgraphics.dll" => hit("XDeviceCreate", Need::Medium),
        "bcrypt.dll" | "ncrypt.dll" | "secur32.dll" | "crypt32.dll" => Provider::Windows("bcrypt.dll"),
        "advapi32.dll" => Provider::Windows("advapi32.dll"),
        "user32.dll" => Provider::Windows("user32.dll"),
        "gdi32.dll" => Provider::Windows("gdi32.dll"),
        "winmm.dll" => Provider::Windows("winmm.dll"),
        "mfplat.dll" => Provider::Windows("mfplat.dll"),
        "dwmapi.dll" => Provider::Windows("dwmapi.dll"),
        // The UCRT & Core API-set forwarder shims.
        _ if n.starts_with("api-ms-win-crt-") => Provider::Windows("ucrtbase.dll"),
        _ if n.starts_with("api-ms-win-core-") => Provider::Windows("kernel32.dll"),
        _ if n.starts_with("api-ms-win-security-") || n.starts_with("api-ms-win-eventing-") => Provider::Windows("advapi32.dll"),
        _ if n.starts_with("xaudio2_") => Provider::Windows("xaudio2_9.dll"),
        _ => return None,
    })
}

/// Names of the Xbox-only libraries, for a summary of what a game still needs from us.
pub fn xbox_only() -> [&'static str; 12] {
    [
        "xgameruntime.dll", "xgamefeature.dll", "xboxone.dll", "xboxservices.dll", "xboxnet.dll", "xstorage.dll", "xuser.dll", "xinput.dll", "x3daudio.dll", "xaudio2.dll", "xapofx.dll", "xact3.dll",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_the_common_libraries_to_windows() {
        assert_eq!(provider("kernelx.dll"), Some(Provider::Windows("kernel32.dll")));
        assert_eq!(provider("D3D12_X.DLL"), Some(Provider::Windows("d3d12.dll")));
        assert_eq!(provider("d3d11_x.dll"), Some(Provider::Windows("d3d11.dll")));
        assert_eq!(provider("api-ms-win-core-synch-l1-2-0.dll"), Some(Provider::Windows("kernelbase.dll")));
        assert_eq!(provider("api-ms-win-crt-runtime-l1-1-0.dll"), Some(Provider::Windows("ucrtbase.dll")));
    }

    #[test]
    fn xbox_only_libraries_are_shims_or_unavailable() {
        assert!(matches!(provider("xgameruntime.dll"), Some(Provider::Shim { .. })));
        assert!(matches!(provider("xgameplatform.dll"), Some(Provider::Shim { .. })));
        assert!(matches!(provider("xg_x.dll"), Some(Provider::Shim { .. })));
        assert!(matches!(provider("pixevent.dll"), Some(Provider::Shim { .. })));
        assert_eq!(provider("xboxnet.dll"), Some(Provider::Unavailable));
        assert_eq!(provider("nonsense.dll"), None);
    }

    #[test]
    fn input_and_audio_use_the_real_windows_libraries() {
        assert_eq!(provider("xinput.dll"), Some(Provider::Windows("xinput1_4.dll")));
        assert_eq!(provider("XAudio2_9.dll"), Some(Provider::Windows("xaudio2_9.dll")));
        assert_eq!(provider("xaudio2_7.dll"), Some(Provider::Windows("xaudio2_9.dll")));
    }

    #[test]
    fn every_listed_xbox_library_is_known() {
        for d in xbox_only() {
            assert!(provider(d).is_some(), "{d} missing from the table");
        }
    }
}
