//! NIDs: Sony identifies every library function by a 64-bit hash of its name. The hash is
//! the first 8 bytes of SHA-1(name + a fixed 16-byte salt), byte-reversed, written in base64 with `-` in
//! place of `/` (11 characters).

use crate::sha1::sha1;

const SALT: [u8; 16] = [0x51, 0x8D, 0x64, 0xA6, 0x35, 0xDE, 0xD8, 0xC1, 0xE6, 0xB0, 0x39, 0xB1, 0xC3, 0xE5, 0x52, 0x30];
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+-";

/// The 11-character NID of a function name.
pub fn nid(name: &str) -> String {
    let mut data = name.as_bytes().to_vec();
    data.extend_from_slice(&SALT);
    let h = sha1(&data);
    let mut out = String::new();
    // The first 8 bytes are read as a little-endian number and written big-endian.
    let mut b: [u8; 8] = h[..8].try_into().unwrap();
    b.reverse();
    for chunk in b.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        let chars = match chunk.len() {
            3 => 4,
            2 => 3,
            _ => 2,
        };
        for i in 0..chars {
            out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// Value of one base64 digit (used for the library / module numbers inside symbol names).
pub fn b64_value(c: u8) -> Option<u32> {
    B64.iter().position(|&x| x == c).map(|p| p as u32)
}

/// Decodes a 1-3 character base64 number.
pub fn b64_number(s: &str) -> Option<u32> {
    s.bytes().try_fold(0u32, |acc, c| Some(acc * 64 + b64_value(c)?))
}

/// System functions we recognise by name: the C library and the kernel services nearly
/// every game calls. Anything else is reported by its hash.
pub const KNOWN: &[&str] = &[
    "sceRtcGetCurrentTick", "sceRtcSetTick", "sceRtcGetTickResolution", "sceUserServiceGetUserName",
    "sceFiosFileExistsSync", "sceFiosFileGetSizeSync", "sceFiosDirectoryCreateSync",
    "sceKernelGetdents", "getdents", "fstat", "_fstat",
    "sceFiosDirectoryExistsSync", "sceFiosStatSync",
    "pthread_cond_timedwait", "pthread_setschedparam", "scePthreadSetschedparam",
    "pthread_create_name_np", "pthread_attr_setstacksize", "pthread_attr_getstacksize", "scePthreadAttrGetstacksize",
    "sceFiosFHPreadSync",
    "sceFiosFHGetOpenParams", "sceFiosFHGetPath",
    "sceSysmoduleUnloadModule",
    "sceFiosIsValidHandle", "scePlayGoTerminate",
    "sceFiosFHReadSync", "sceFiosFHSeek", "sceFiosFHTell", "sceFiosTerminate",
    "scePlayGoInitialize", "scePlayGoOpen", "scePlayGoGetLocus",
    "pthread_mutexattr_setprotocol",
    "sceKernelReserveVirtualRange", "sceKernelBatchMap",
    "_exit", "usleep", "scePthreadGetthreadid", "sceFiosFHOpenWithModeSync", "sceFiosFHOpenSync", "sceFiosFHGetSize", "sceFiosFHCloseSync",
    "sceFiosInitialize", "sceAppContentInitialize", "sceAppContentTemporaryDataFormat", "sceAppContentTemporaryDataGetAvailableSpaceKb", "sceAppContentTemporaryDataMount2",
    "sceMsgDialogInitialize", "sceMsgDialogOpen", "sceMsgDialogGetResult", "sceMsgDialogUpdateStatus", "sceMsgDialogTerminate", "sceKernelDebugRaiseExceptionOnReleaseMode",
    "sceSystemServiceLaunchApp", "sceGnmSetPsShader350",
    "sceKernelIsAddressSanitizerEnabled", "sceKernelGetSanitizerMallocReplaceExternal",
    "pthread_attr_init", "pthread_attr_destroy", "pthread_attr_get_np",
    "pthread_rwlock_destroy", "pthread_rwlock_rdlock", "pthread_rwlock_tryrdlock", "pthread_rwlock_trywrlock",
    "scePthreadRwlockInit", "scePthreadRwlockDestroy", "scePthreadRwlockRdlock", "scePthreadRwlockWrlock", "scePthreadRwlockTryrdlock", "scePthreadRwlockTrywrlock", "scePthreadRwlockUnlock",
    "_sceKernelSetThreadDtors", "_sceKernelSetThreadAtexitCount", "_sceKernelSetThreadAtexitReport", "_sceKernelRtldSetApplicationHeapAPI",
    "sceKernelGetSanitizerNewReplaceExternal", "sceLibcHeapGetTraceInfo", "sceCoredumpRegisterCoredumpHandler",
    "scePthreadAttrGet", "scePthreadAttrGetaffinity", "pthread_rwlock_init", "pthread_rwlock_wrlock", "pthread_rwlock_unlock",
    "sceKernelAvailableDirectMemorySize", "sceKernelMemoryPoolExpand", "sceKernelMemoryPoolReserve", "sceKernelMemoryPoolCommit", "sceKernelMemoryPoolDecommit",
    "scePthreadMutexattrInit", "scePthreadMutexattrDestroy", "scePthreadMutexattrSettype",
    "sceKernelGetProcParam", "sceKernelDebugRaiseException",
    "sceRandomGetRandomNumber",
    "__tls_get_addr", "__progname",
    "sceVideoOutIsFlipPending", "sceUserServiceGetForegroundUser", "sceKernelSendNotificationRequest",
    "sceGnmDrawInitDefaultHardwareState350", "sceGnmDispatchInitDefaultHardwareState",
    "sceGnmSetCsShaderWithModifier",
    "sceGnmInsertPushMarker", "sceGnmInsertPopMarker", "sceGnmAddEqEvent", "sceGnmDeleteEqEvent", "sceGnmRegisterOwner",
    "sceVideoOutAddVblankEvent", "sceVideoOutGetDeviceCapabilityInfo_",
    "sceKernelAddUserEventEdge", "sceKernelDeleteUserEvent",
    "open", "_open", "read", "_read", "write", "_write", "readv", "_readv", "writev", "_writev", "lseek", "_lseek", "close", "_close", "ioctl", "_ioctl", "munmap", "pthread_mutexattr_init", "pthread_mutexattr_destroy", "pthread_mutexattr_settype",
    "__error", "__errno_location",
    "sceRtcGetCurrentClockLocalTime",
    "scePthreadKeyCreate", "scePthreadKeyDelete", "scePthreadGetspecific", "scePthreadSetspecific", "sceSystemServiceParamGetInt",
    "pthread_key_create", "pthread_key_delete", "pthread_getspecific", "pthread_setspecific", "mmap",
    "malloc", "free", "calloc", "realloc", "memalign", "posix_memalign", "memcpy", "memmove", "memset", "memcmp", "strlen", "strcpy", "strncpy", "strcmp", "strncmp", "strcat", "strchr", "strrchr", "strstr", "printf", "sprintf", "snprintf", "vsnprintf", "puts", "fopen", "fclose", "fread", "fwrite", "fseek", "ftell", "abort", "exit", "atexit", "rand", "srand", "qsort", "bsearch", "atoi", "strtol", "strtoul", "strtod", "sin", "cos", "tan", "sqrt", "pow", "floor", "ceil", "fabs", "atan2", "exp", "log", "__stack_chk_fail", "__stack_chk_guard", "_init_env", "setlocale", "time", "gettimeofday", "clock_gettime", "nanosleep", "pthread_create", "pthread_join", "pthread_mutex_init", "pthread_mutex_lock", "pthread_mutex_unlock", "pthread_cond_init", "pthread_cond_wait", "pthread_cond_signal", "pthread_self", "scePthreadCreate", "scePthreadJoin", "scePthreadMutexInit", "scePthreadMutexLock", "scePthreadMutexUnlock", "scePthreadCondInit", "scePthreadCondWait", "scePthreadCondSignal", "scePthreadSelf", "scePthreadYield", "sceKernelUsleep", "sceKernelSleep", "sceKernelGetProcessTime", "sceKernelGetProcessTimeCounter", "sceKernelGetProcessTimeCounterFrequency", "sceKernelReadTsc", "sceKernelAllocateDirectMemory", "sceKernelAllocateMainDirectMemory", "sceKernelMapDirectMemory", "sceKernelMapNamedDirectMemory", "sceKernelMapFlexibleMemory", "sceKernelReleaseDirectMemory", "sceKernelMunmap", "sceKernelMmap", "sceKernelVirtualQuery", "sceKernelGetDirectMemorySize", "sceKernelAvailableFlexibleMemorySize", "sceKernelOpen", "sceKernelClose", "sceKernelRead", "sceKernelWrite", "sceKernelLseek", "sceKernelStat", "sceKernelFstat", "sceKernelMkdir", "sceKernelUnlink", "sceKernelCreateEqueue", "sceKernelDeleteEqueue", "sceKernelWaitEqueue", "sceKernelAddUserEvent", "sceKernelTriggerUserEvent", "sceKernelCreateSema", "sceKernelDeleteSema", "sceKernelWaitSema", "sceKernelSignalSema", "sceKernelCreateEventFlag", "sceKernelSetEventFlag", "sceKernelWaitEventFlag", "sceKernelGetModuleInfo", "sceKernelLoadStartModule", "sceKernelDlsym", "sceKernelIsNeoMode", "sceKernelGetCompiledSdkVersion", "sceKernelDebugOutText", "sceSysmoduleLoadModule", "sceSysmoduleIsLoaded", "sceVideoOutOpen", "sceVideoOutClose", "sceVideoOutRegisterBuffers", "sceVideoOutSubmitFlip", "sceVideoOutSetFlipRate", "sceVideoOutGetFlipStatus", "sceVideoOutAddFlipEvent", "sceVideoOutSetBufferAttribute", "sceVideoOutGetResolutionStatus", "sceGnmSubmitCommandBuffers", "sceGnmSubmitAndFlipCommandBuffers", "sceGnmSubmitDone", "sceGnmDrawIndex", "sceGnmDrawIndexAuto", "sceGnmDispatchDirect", "sceGnmSetVsShader", "sceGnmSetPsShader", "sceGnmSetCsShader", "sceGnmInsertWaitFlipDone", "sceGnmDingDong", "sceGnmMapComputeQueue", "sceUserServiceInitialize", "sceUserServiceGetInitialUser", "sceUserServiceGetEvent", "scePadInit", "scePadOpen", "scePadClose", "scePadRead", "scePadReadState", "scePadSetLightBar", "scePadSetVibration", "sceAudioOutInit", "sceAudioOutOpen", "sceAudioOutOutput", "sceAudioOutClose", "sceSaveDataInitialize", "sceSaveDataMount", "sceSaveDataUmount", "sceSaveDataSetParam", "sceNetInit", "sceNetSocket", "sceNetTerm", "sceSystemServiceGetStatus", "sceSystemServiceReceiveEvent", "sceSystemServiceHideSplashScreen", "sceCommonDialogInitialize", "scePthreadMutexTrylock", "scePthreadMutexDestroy", "pthread_mutex_destroy", "pthread_mutex_trylock", "pthread_cond_broadcast", "scePthreadCondBroadcast", "scePthreadCondTimedwait", "pthread_cond_destroy", "scePthreadCondDestroy", "pthread_exit", "scePthreadExit", "pthread_detach", "scePthreadDetach", "scePthreadAttrInit", "scePthreadAttrDestroy", "scePthreadAttrSetstacksize", "scePthreadAttrSetdetachstate", "scePthreadAttrSetschedparam", "scePthreadSetaffinity", "scePthreadRename", "sched_yield", 
];

/// Maps a NID back to a function name if it is one we know.
pub fn lookup(nid_str: &str) -> Option<&'static str> {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static MAP: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| KNOWN.iter().map(|n| (nid(n), *n)).collect()).get(nid_str).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_published_nids() {
        // Values published on the PlayStation developer wiki.
        assert_eq!(nid("malloc"), "gQX+4GDQjpM");
        assert_eq!(nid("free"), "tIhsqj0qsFE");
        assert_eq!(nid("memcpy"), "Q3VBxCXhUHs");
        assert_eq!(nid("__stack_chk_fail"), "Ou3iL1abvng");
    }

    #[test]
    fn title_launch_import_is_named_for_startup_diagnostics() {
        assert_eq!(nid("sceSystemServiceLaunchApp"), "l4FB3wNa-Ac");
        assert_eq!(lookup("l4FB3wNa-Ac"), Some("sceSystemServiceLaunchApp"));
    }

    #[test]
    fn observed_graphics_imports_are_named_without_claiming_support() {
        for (hash, name) in [
            ("yb2cRhagD1I", "sceGnmDrawInitDefaultHardwareState350"),
            ("nF6bFRUBRAU", "sceGnmDispatchInitDefaultHardwareState"),
            ("W1Etj-jlW7Y", "sceGnmInsertPushMarker"),
            ("7qZVNgEu+SY", "sceGnmInsertPopMarker"),
            ("Xru92wHJRmg", "sceVideoOutAddVblankEvent"),
        ] {
            assert_eq!(nid(name), hash);
            assert_eq!(lookup(hash), Some(name));
        }
    }

    #[test]
    fn base64_numbers() {
        assert_eq!(b64_number("A"), Some(0));
        assert_eq!(b64_number("B"), Some(1));
        assert_eq!(b64_number("BA"), Some(64));
    }
}
