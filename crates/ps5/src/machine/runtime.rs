use super::*;

mod ranges;
mod playgo;
mod rtc;

fn clock_bytes(nanoseconds: u64) -> [u8; 16] {
    rtc::date_from_ticks(rtc::UNIX_EPOCH_TICKS + 1700000000 * 1000000 + nanoseconds / 1000).unwrap()
}

#[derive(Default)]
pub(super) struct Runtime {
    user_service_initialized: bool,
    heap_trace: Option<u64>,
    playgo_buffer: Option<(u64, u64)>,
    application_heap: Option<u64>,
    native_mutexes: std::collections::HashMap<u64, u64>,
    virtual_ranges: Vec<(u64, u64)>,
    pub(super) direct_mappings: Vec<(u64, u64, u64)>,
    direct_backing: HashMap<u64, u64>,
}

impl Machine {
    fn register_application_heap(&mut self) -> u64 {
        let address = self.arg(0);
        if address == 0 { return 0x8002_0016; }
        let mut callbacks = [0u8; 80];
        if !self.mem.read(address, &mut callbacks) { return 0x8002_000E; }
        for (index, bytes) in callbacks.chunks_exact(8).enumerate() {
            let callback = u64::from_le_bytes(bytes.try_into().unwrap());
            if callback == 0 {
                if index < 2 { return 0x8002_0016; }
            } else if self.mem.phys(callback).is_none() {
                return 0x8002_000E;
            }
        }
        self.runtime.application_heap = Some(address);
        0
    }

    fn initialize_native_mutex(&mut self) -> u64 {
        let output = self.arg(0);
        if output == 0 { return 0x8002_0016; }
        let mut previous = [0u8; 8];
        if !self.mem.read(output, &mut previous) { return 0x8002_000E; }
        if self.runtime.native_mutexes.contains_key(&output) || self.mutex_is_locked(output) {
            return 0x8002_0010;
        }
        let object = self.heap_alloc(256, 16);
        if object == 0 { return 0x8002_000C; }
        if !self.mem.write(object, &[0; 256]) || !self.mem.write_u64(output, object) {
            self.sizes.remove(&object);
            return 0x8002_000E;
        }
        self.runtime.native_mutexes.insert(output, object);
        0
    }

    fn destroy_native_mutex(&mut self) -> u64 {
        let output = self.arg(0);
        let Some(&object) = self.runtime.native_mutexes.get(&output) else { return 0x8002_0016; };
        if self.mutex_is_locked(output) { return 0x8002_0010; }
        if self.mem.read_u64(output) != object || !self.mem.write_u64(output, 0) { return 0x8002_000E; }
        self.runtime.native_mutexes.remove(&output);
        self.sizes.remove(&object);
        0
    }

    fn heap_trace_info(&mut self) -> u64 {
        let output = self.arg(0);
        if output == 0 { return 0x8002_0016; }
        let mut info = [0u8; 32];
        if !self.mem.read(output, &mut info) { return 0x8002_000E; }
        if u64::from_le_bytes(info[..8].try_into().unwrap()) < info.len() as u64 { return 0x8002_0016; }
        let storage = match self.runtime.heap_trace {
            Some(storage) => storage,
            None => {
                let storage = self.heap_alloc(520, 16);
                if storage == 0 { return 0x8002_000C; }
                self.mem.write(storage, &[0u8; 520]);
                self.runtime.heap_trace = Some(storage);
                storage
            }
        };
        info[12..16].fill(0);
        info[16..24].copy_from_slice(&storage.to_le_bytes());
        info[24..32].copy_from_slice(&(storage + 8).to_le_bytes());
        if self.mem.write(output, &info) { 0 } else { 0x8002_000E }
    }

    fn system_setting(&mut self) -> u64 {
        let value: i32 = match self.arg(0) as u32 {
            1 | 3 | 1000 => 1,
            2 | 4 | 5 | 7 => 0,
            _ => return 0x80A1_0003,
        };
        let mut previous = [0; 4];
        if self.arg(1) == 0 || !self.mem.read(self.arg(1), &mut previous) { return 0x80A1_0003; }
        if self.mem.write(self.arg(1), &value.to_le_bytes()) { 0 } else { 0x80A1_0003 }
    }

    fn local_user_name(&mut self) -> u64 {
        if self.arg(0) != 1 { return 0x80960009; }
        let name = b"Player1\0";
        if self.arg(2) < name.len() as u64 { return 0x8096000A; }
        let output = self.arg(1);
        if output == 0 || !self.mem.read(output, &mut [0; 8]) { return 0x80960005; }
        if self.mem.write(output, name) { 0 } else { 0x80960005 }
    }

    fn initialize_user_service(&mut self) -> u64 {
        if self.runtime.user_service_initialized { return 0x80960003; }
        let parameters = self.arg(0);
        if parameters != 0 && !self.mem.read(parameters, &mut [0; 4]) { return 0x80960005; }
        self.runtime.user_service_initialized = true;
        0
    }

    fn terminate_user_service(&mut self) -> u64 {
        if !self.runtime.user_service_initialized { return 0x80960002; }
        self.runtime.user_service_initialized = false;
        0
    }

    fn login_user_id_list(&mut self) -> u64 {
        if !self.runtime.user_service_initialized { return 0x80960002; }
        let output = self.arg(0);
        if output == 0 || !self.mem.read(output, &mut [0; 16]) { return 0x80960005; }
        let mut list = [0u8; 16];
        list[0..4].copy_from_slice(&1i32.to_le_bytes());
        for slot in 1..4 {
            list[slot * 4..slot * 4 + 4].copy_from_slice(&(-1i32).to_le_bytes());
        }
        if self.mem.write(output, &list) { 0 } else { 0x80960005 }
    }

    fn user_color(&mut self) -> u64 {
        if self.arg(0) != 1 { return 0x80960009; }
        let output = self.arg(1);
        if output == 0 || !self.mem.read(output, &mut [0; 4]) { return 0x80960005; }
        if self.mem.write(output, &0u32.to_le_bytes()) { 0 } else { 0x80960005 }
    }

    fn pad_controller_information(&mut self) -> u64 {
        let handle = self.arg(0) as i32;
        let output = self.arg(1);
        if handle != 1 { return 0x8092_0004; }
        if output == 0 || !self.mem.read(output, &mut [0; 36]) { return 0x8092_0006; }
        // ScePadControllerInformation: touchpad (1920x943, density 44.86), stickDeadZone (13, 13),
        // connectionType (0=local), connectedCount (1), connected (1), deviceClass (0=Standard).
        let mut info = [0u8; 36];
        info[0..4].copy_from_slice(&44.86f32.to_le_bytes());
        info[4..8].copy_from_slice(&1920u32.to_le_bytes());
        info[8..12].copy_from_slice(&943u32.to_le_bytes());
        info[12] = 13;
        info[13] = 13;
        info[14] = 0; // local
        info[15] = 1; // connectedCount
        info[16] = 1; // connected
        info[20..24].copy_from_slice(&0u32.to_le_bytes()); // standard controller
        if self.mem.write(output, &info) { 0 } else { 0x8092_0006 }
    }

    fn poll_user_event(&mut self) -> u64 {
        if !self.runtime.user_service_initialized { return 0x80960002; }
        let output = self.arg(0);
        if output == 0 || !self.mem.read(output, &mut [0; 8]) { return 0x80960005; }
        0x80960007
    }

    fn current_local_clock(&mut self) -> u64 {
        let output = self.arg(0);
        let mut previous = [0; 16];
        if output == 0 || !self.mem.read(output, &mut previous) { return 0x80B5_0002; }
        if self.mem.write(output, &clock_bytes(self.cpu.tsc)) { 0 } else { 0x80B5_0002 }
    }

    fn set_mutex_protocol(&mut self) -> u64 {
        let address = self.arg(0);
        let protocol = self.arg(1);
        let mut attribute = [0; 4];
        if address == 0 || !self.mem.read(address, &mut attribute) || protocol > 2 { return 22; }
        if protocol != 0 { return 45; }
        0
    }

    pub(super) fn call_runtime(&mut self, name: &str) -> bool {
        if self.call_playgo(name) || self.call_rtc(name) { return true; }
        match name {
            "sceUserServiceInitialize" => {
                let result = self.initialize_user_service();
                self.ret(result);
            }
            "sceUserServiceTerminate" => {
                let result = self.terminate_user_service();
                self.ret(result);
            }
            "sceUserServiceGetLoginUserIdList" => {
                let result = self.login_user_id_list();
                self.ret(result);
            }
            "sceUserServiceGetUserColor" => {
                let result = self.user_color();
                self.ret(result);
            }
            "sceUserServiceGetEvent" => {
                let result = self.poll_user_event();
                self.ret(result);
            }
            "sceUserServiceGetUserName" => {
                let result = self.local_user_name();
                self.ret(result);
            }
            "scePadGetControllerInformation" => {
                let result = self.pad_controller_information();
                self.ret(result);
            }
            "scePadClose" | "scePadSetLightBar" | "scePadResetLightBar" | "scePadSetVibration" => self.ret(0),
            "sceSystemServiceIsScreenSaverOn" => {
                if self.arg(0) != 0 {
                    let _ = self.mem.write(self.arg(0), &[0u8; 4]);
                }
                self.ret(0);
            }
            "sceKernelGetCpuTemperature" => {
                let out = self.arg(0);
                let ok = out != 0 && self.mem.write(out, &45i32.to_le_bytes());
                self.ret(if ok { 0 } else { 0x8002_000E });
            }
            "sceKernelGetSocSensorTemperature" => {
                let out = self.arg(1);
                let ok = out != 0 && self.mem.write(out, &48i32.to_le_bytes());
                self.ret(if ok { 0 } else { 0x8002_000E });
            }
            "sceKernelGetCurrentCpu" => self.ret(0),
            "sceAudioOutOpen" => self.ret(1),
            "sceAudioOutClose" | "sceAudioOutSetVolume" => self.ret(0),
            "sceAudioOutOutput" => {
                let handle = self.arg(0) as i32;
                self.ret(if handle > 0 { 256 } else { 0x8026_0003 });
            }
            "sceAudioOutOutputs" => self.ret(0),
            "sceSaveDataInitialize3" | "sceSaveDataTerminate" => self.ret(0),
            "sceNetInit" | "sceNetTerm" | "sceNetCtlInit" | "sceNetCtlTerm" => self.ret(0),
            "sceNetCtlGetState" => {
                let out = self.arg(0);
                let ok = out != 0 && self.mem.write(out, &0i32.to_le_bytes());
                self.ret(if ok { 0 } else { 0x8041_2102 });
            }
            "sceNpTrophyCreateContext" => {
                let out = self.arg(0);
                let ok = out != 0 && self.mem.write(out, &1i32.to_le_bytes());
                self.ret(if ok { 0 } else { 0x8055_1602 });
            }
            "sceNpTrophyCreateHandle" => {
                let out = self.arg(0);
                let ok = out != 0 && self.mem.write(out, &1i32.to_le_bytes());
                self.ret(if ok { 0 } else { 0x8055_1602 });
            }
            "sceNpTrophyRegisterContext" | "sceNpTrophyDestroyHandle" | "sceNpTrophyDestroyContext" => self.ret(0),
            "sceSysmoduleLoadModuleInternal" | "sceSysmoduleUnloadModuleInternal" => self.ret(0),
            "sceSystemServiceIsShellUiForeground" | "sceCommonDialogIsUsed" => self.ret(0),
            "sceSystemServiceGetDisplaySafeAreaInfo" => {
                let out = self.arg(0);
                let ok = out != 0 && self.mem.write(out, &0.9f32.to_le_bytes());
                self.ret(if ok { 0 } else { 0x80A1_0003 });
            }
            "sceMsgDialogInitialize" | "sceMsgDialogTerminate" | "sceMsgDialogGetStatus" => self.ret(0),
            "scePadSetMotionSensorState" | "scePadSetTiltCorrectionState" | "scePadSetAngularVelocityDeadbandState" => self.ret(0),
            "sceKernelGetIdPs" | "sceKernelGetOpenPsIdForSystem" => {
                let out = self.arg(0);
                let id: [u8; 16] = [0x00, 0x00, 0x00, 0x01, 0x00, 0x84, 0x00, 0x07, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];
                let ok = out != 0 && self.mem.write(out, &id);
                self.ret(if ok { 0 } else { 0x8002_000E });
            }
            "sceSaveDataDialogInitialize" | "sceSaveDataDialogTerminate" | "sceSaveDataDialogGetStatus" => self.ret(0),
            "sceNpCheckCallback" => self.ret(0),
            "sceNpGetState" => {
                let out = self.arg(1);
                let ok = out != 0 && self.mem.write(out, &0i32.to_le_bytes());
                self.ret(if ok { 0 } else { 0x8055_0002 });
            }
            "sceNpGetOnlineId" => {
                let out = self.arg(1);
                let mut id = [0u8; 20];
                id[..7].copy_from_slice(b"Player1");
                let ok = out != 0 && self.mem.write(out, &id);
                self.ret(if ok { 0 } else { 0x8055_0002 });
            }
            "sceSystemServiceParamGetString" => {
                let out = self.arg(1);
                let cap = self.arg(2);
                let ok = out != 0 && cap > 0 && self.mem.write(out, &[0]);
                self.ret(if ok { 0 } else { 0x80A1_0003 });
            }
            "sceSystemServiceLoadExec" => self.ret(0x80A1_0003),
            "sceKernelGetAppCategoryType" => {
                let out = self.arg(0);
                let ok = out != 0 && self.mem.write(out, &0i32.to_le_bytes());
                self.ret(if ok { 0 } else { 0x8002_000E });
            }
            "sceVideoOutGetVblankStatus" => {
                let out = self.arg(1);
                let mut st = [0u8; 32];
                let count = self.cpu.tsc / 16_666_667;
                st[0..8].copy_from_slice(&count.to_le_bytes());
                st[8..16].copy_from_slice(&(self.cpu.tsc / 1000).to_le_bytes());
                let ok = out != 0 && self.mem.write(out, &st);
                self.ret(if ok { 0 } else { 0x8029_0002 });
            }
            "sceVideoOutWaitVblank" => {
                self.cpu.tsc = self.cpu.tsc.wrapping_add(16_666_667);
                self.ret(0);
            }
            "sceImeDialogInit" | "sceImeDialogTerm" | "sceImeDialogGetStatus" => self.ret(0),
            "sceErrorDialogInitialize" | "sceErrorDialogTerminate" | "sceErrorDialogGetStatus" => self.ret(0),
            "scePadSetLightBarBaseBrightness" => self.ret(0),
            "scePadGetHandle" => self.ret(1),
            "sceKernelGetProcessName" => {
                let out = self.arg(0);
                let ok = out != 0 && self.mem.write(out, b"eboot.bin\0");
                self.ret(if ok { 0 } else { 0x8002_000E });
            }
            "sceKernelCheckReachability" => self.ret(0),
            "pthread_mutexattr_setprotocol" => {
                let result = self.set_mutex_protocol();
                self.ret(result);
            }
            "sceKernelReserveVirtualRange" => {
                let result = self.reserve_virtual_range();
                self.ret(result);
            }
            "sceKernelBatchMap" => {
                let result = self.batch_map();
                self.ret(result);
            }
            "_sceKernelRtldSetApplicationHeapAPI" => {
                let result = self.register_application_heap();
                self.ret(result);
            }
            "scePthreadMutexInit" => {
                let result = self.initialize_native_mutex();
                self.ret(result);
            }
            "scePthreadMutexDestroy" => {
                let result = self.destroy_native_mutex();
                self.ret(result);
            }
            "sceRtcGetCurrentClockLocalTime" => {
                let result = self.current_local_clock();
                self.ret(result);
            }
            "sceSystemServiceParamGetInt" => {
                let result = self.system_setting();
                self.ret(result);
            }
            "sceLibcHeapGetTraceInfo" => {
                let result = self.heap_trace_info();
                self.ret(result);
            }
            "sceKernelIsAddressSanitizerEnabled" | "sceKernelGetSanitizerMallocReplaceExternal" | "sceKernelGetSanitizerNewReplaceExternal" => self.ret(0),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_event_polling_reports_an_empty_queue_without_fabricating_events() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let output = machine.heap_alloc(16, 16);
        machine.mem.write(output, &[0xCC; 16]);
        machine.cpu.r[7] = output;
        assert_eq!(machine.poll_user_event(), 0x80960002);
        machine.cpu.r[7] = u64::MAX;
        assert_eq!(machine.initialize_user_service(), 0x80960005);
        machine.cpu.r[7] = 0;
        assert_eq!(machine.initialize_user_service(), 0);
        assert_eq!(machine.initialize_user_service(), 0x80960003);
        assert_eq!(machine.poll_user_event(), 0x80960005);
        machine.cpu.r[7] = output;
        for _ in 0..3 { assert_eq!(machine.poll_user_event(), 0x80960007); }
        assert_eq!(machine.mem.read_u64(output), 0xCCCCCCCCCCCCCCCC);
        assert_eq!(machine.mem.read_u64(output + 8), 0xCCCCCCCCCCCCCCCC);
    }

    #[test]
    fn local_user_names_obey_the_active_virtual_user_and_buffer_size() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let output = machine.heap_alloc(16, 16);
        machine.mem.write(output, &[0xCC; 16]);
        machine.cpu.r[7] = 1;
        machine.cpu.r[6] = output;
        machine.cpu.r[2] = 7;
        assert_eq!(machine.local_user_name(), 0x8096000A);
        assert_eq!(machine.mem.read_u64(output), 0xCCCCCCCCCCCCCCCC);
        machine.cpu.r[2] = 8;
        assert_eq!(machine.local_user_name(), 0);
        assert_eq!(machine.mem.read_cstr(output), "Player1");
        assert_eq!(machine.mem.read_u64(output + 8), 0xCCCCCCCCCCCCCCCC);
        machine.cpu.r[7] = 2;
        assert_eq!(machine.local_user_name(), 0x80960009);
        machine.cpu.r[7] = 1;
        machine.cpu.r[6] = u64::MAX;
        assert_eq!(machine.local_user_name(), 0x80960005);
    }

    #[test]
    fn login_user_list_color_terminate_and_pad_info_behave_consistently() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let buf = machine.heap_alloc(64, 16);
        machine.cpu.r[7] = buf;
        assert_eq!(machine.login_user_id_list(), 0x80960002);
        machine.cpu.r[7] = 0;
        assert_eq!(machine.initialize_user_service(), 0);
        machine.cpu.r[7] = buf;
        assert_eq!(machine.login_user_id_list(), 0);
        let mut ids = [0u8; 16];
        assert!(machine.mem.read(buf, &mut ids));
        assert_eq!(i32::from_le_bytes(ids[0..4].try_into().unwrap()), 1);
        assert_eq!(i32::from_le_bytes(ids[4..8].try_into().unwrap()), -1);

        machine.cpu.r[7] = 1;
        machine.cpu.r[6] = buf;
        assert_eq!(machine.user_color(), 0);
        assert_eq!(machine.terminate_user_service(), 0);
        assert_eq!(machine.terminate_user_service(), 0x80960002);

        machine.cpu.r[7] = 1;
        machine.cpu.r[6] = buf;
        assert_eq!(machine.pad_controller_information(), 0);
        let mut info = [0u8; 36];
        assert!(machine.mem.read(buf, &mut info));
        assert_eq!(u32::from_le_bytes(info[4..8].try_into().unwrap()), 1920);
        assert_eq!(info[16], 1);
    }

    #[test]
    fn mutex_protocol_rejects_priority_modes_the_scheduler_does_not_support() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let attribute = machine.heap_alloc(8, 8);
        machine.mem.write_u64(attribute, 0xCCCCCCCC00000002);
        machine.cpu.r[7] = attribute;
        machine.cpu.r[6] = 0;
        assert_eq!(machine.set_mutex_protocol(), 0);
        for protocol in [1, 2] {
            machine.cpu.r[6] = protocol;
            assert_eq!(machine.set_mutex_protocol(), 45);
        }
        machine.cpu.r[6] = 3;
        assert_eq!(machine.set_mutex_protocol(), 22);
        assert_eq!(machine.mem.read_u64(attribute), 0xCCCCCCCC00000002);
        machine.cpu.r[7] = u64::MAX;
        machine.cpu.r[6] = 0;
        assert_eq!(machine.set_mutex_protocol(), 22);
    }

    #[test]
    fn native_mutex_initialization_returns_writable_storage_and_preserves_output_guards() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let storage = machine.heap_alloc(24, 8);
        machine.mem.write(storage, &[0xCC; 24]);
        let output = storage + 8;
        machine.cpu.r[7] = output;
        assert!(machine.call_runtime("scePthreadMutexInit"));
        assert_eq!(machine.cpu.r[0], 0);
        let object = machine.mem.read_u64(output);
        assert_ne!(object, 0);
        assert!(machine.mem.write(object + 0x20, &0x100u32.to_le_bytes()));
        assert_eq!(machine.mem.read_u64(storage), 0xCCCCCCCCCCCCCCCC);
        assert_eq!(machine.mem.read_u64(storage + 16), 0xCCCCCCCCCCCCCCCC);
        machine.call_runtime("scePthreadMutexInit");
        assert_eq!(machine.cpu.r[0], 0x80020010);
        assert_eq!(machine.mem.read_u64(output), object);
        machine.mutex_lock(output, true);
        machine.call_runtime("scePthreadMutexDestroy");
        assert_eq!(machine.cpu.r[0], 0x80020010);
        machine.mutex_unlock(output);
        machine.call_runtime("scePthreadMutexDestroy");
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.mem.read_u64(output), 0);
        machine.cpu.r[7] = u64::MAX;
        machine.call_runtime("scePthreadMutexInit");
        assert_eq!(machine.cpu.r[0], 0x8002000E);
        assert!(machine.runtime.native_mutexes.is_empty());
    }

    #[test]
    fn application_heap_registration_rejects_invalid_callback_tables() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let table = machine.heap_alloc(80, 8);
        for index in 0..10 { machine.mem.write_u64(table + index * 8, machine.entry); }
        machine.cpu.r[7] = table;
        assert!(machine.call_runtime("_sceKernelRtldSetApplicationHeapAPI"));
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.runtime.application_heap, Some(table));
        let invalid_table = machine.heap_alloc(80, 8);
        for index in 0..10 { machine.mem.write_u64(invalid_table + index * 8, machine.entry); }
        machine.mem.write_u64(invalid_table, u64::MAX);
        machine.cpu.r[7] = invalid_table;
        machine.call_runtime("_sceKernelRtldSetApplicationHeapAPI");
        assert_eq!(machine.cpu.r[0], 0x8002000E);
        assert_eq!(machine.runtime.application_heap, Some(table));
        machine.cpu.r[7] = 0;
        machine.call_runtime("_sceKernelRtldSetApplicationHeapAPI");
        assert_eq!(machine.cpu.r[0], 0x80020016);
    }

    #[test]
    fn heap_trace_outputs_use_stable_guest_storage_and_preserve_the_callers_header() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let output = 0x50000000;
        machine.mem.map(output, 4096, true, false).unwrap();
        machine.mem.write_u64(output, 32);
        machine.mem.write_u64(output + 8, u64::MAX);
        machine.cpu.r[7] = output;
        assert_eq!(machine.heap_trace_info(), 0);
        assert_eq!(machine.mem.read_u64(output), 32);
        assert_eq!(machine.mem.read_u64(output + 8), u32::MAX as u64);
        let mask = machine.mem.read_u64(output + 16);
        let table = machine.mem.read_u64(output + 24);
        assert_eq!(table, mask + 8);
        assert_eq!(machine.mem.read_u64(mask), 0);
        let mut bytes = [1u8; 512];
        assert!(machine.mem.read(table, &mut bytes));
        assert_eq!(bytes, [0; 512]);
        assert_eq!(machine.heap_trace_info(), 0);
        assert_eq!(machine.mem.read_u64(output + 16), mask);
        machine.mem.write_u64(output, 8);
        assert_eq!(machine.heap_trace_info(), 0x80020016);
        machine.cpu.r[7] = 0;
        assert_eq!(machine.heap_trace_info(), 0x80020016);
    }
    #[test]
    #[ignore]
    fn trace_real_game_runtime_registration() {
        let path = std::env::var_os("HITBOY_PS4_TARGET").expect("Set HITBOY_PS4_TARGET to an unpacked game folder.");
        let mut machine = Machine::load_game(std::path::Path::new(&path)).unwrap();
        let mut calls = 0;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1_000_000 {
            let result = machine.run(1);
            if machine.calls != calls {
                calls = machine.calls;
                if let Some((name, arguments)) = &machine.last_call {
                    if machine.unimplemented.contains(name) && seen.insert(name.clone()) {
                        println!("Unimplemented {name}: arguments {arguments:x?}; caller {:#x}", machine.mem.read_u64(machine.cpu.r[4]));
                        let mut bytes = [0; 64];
                        if machine.mem.read(arguments[0], &mut bytes) { println!("First argument guest bytes: {bytes:02x?}"); }
                    }
                }
            }
            if !matches!(result, Ok(Stop::Running)) { println!("Diagnostic stop: {result:?}"); break; }
        }
        assert!(!seen.is_empty());
        println!("Diagnostic only: unsupported calls retained; no gameplay assertion.");
    }

    #[test]
    fn integer_system_settings_validate_ids_and_preserve_surrounding_memory() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let output = machine.heap_alloc(16, 8);
        machine.mem.write(output, &[0xCC; 16]);
        machine.cpu.r[6] = output + 4;
        for (id, expected) in [(1, 1u32), (2, 0), (3, 1), (4, 0), (5, 0), (7, 0), (1000, 1)] {
            machine.cpu.r[7] = id;
            assert!(machine.call_runtime("sceSystemServiceParamGetInt"));
            assert_eq!(machine.cpu.r[0], 0);
            let mut bytes = [0; 16];
            machine.mem.read(output, &mut bytes);
            assert_eq!(&bytes[..4], &[0xCC; 4]);
            assert_eq!(&bytes[4..8], &expected.to_le_bytes());
            assert_eq!(&bytes[8..], &[0xCC; 8]);
        }
        machine.cpu.r[7] = 6;
        assert_eq!(machine.system_setting(), 0x80A1_0003);
        machine.cpu.r[7] = 1000;
        machine.cpu.r[6] = 0;
        assert_eq!(machine.system_setting(), 0x80A1_0003);
        machine.cpu.r[6] = u64::MAX - 1;
        assert_eq!(machine.system_setting(), 0x80A1_0003);
        assert_eq!(crate::nid::lookup("fZo48un7LK4"), Some("sceSystemServiceParamGetInt"));
    }

    #[test]
    fn local_clock_uses_the_emulated_epoch_and_full_microsecond_field() {
        for (seconds, date) in [(1_700_000_000u64, [2023u16, 11, 14, 22, 13, 20]), (1_709_164_800, [2024, 2, 29, 0, 0, 0]), (4_107_542_400, [2100, 3, 1, 0, 0, 0])] {
            let bytes = clock_bytes((seconds - 1_700_000_000) * 1_000_000_000 + 999_999_000);
            for (index, field) in date.into_iter().enumerate() { assert_eq!(&bytes[index * 2..index * 2 + 2], &field.to_le_bytes()); }
            assert_eq!(&bytes[12..], &999999u32.to_le_bytes());
        }
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let buffer = machine.heap_alloc(24, 8);
        machine.mem.write(buffer, &[0xCC; 24]);
        machine.cpu.r[7] = buffer + 4;
        assert_eq!(machine.current_local_clock(), 0);
        let mut bytes = [0; 24];
        machine.mem.read(buffer, &mut bytes);
        assert_eq!(&bytes[..4], &[0xCC; 4]);
        assert_eq!(&bytes[20..], &[0xCC; 4]);
        machine.cpu.r[7] = 0;
        assert_eq!(machine.current_local_clock(), 0x80B50002);
        machine.cpu.r[7] = u64::MAX - 1;
        assert_eq!(machine.current_local_clock(), 0x80B50002);
        assert_eq!(crate::nid::lookup("ZPD1YOKI+Kw"), Some("sceRtcGetCurrentClockLocalTime"));
    }

    #[test]
    #[ignore]
    fn trace_real_game_memory_requests() {
        let path = std::env::var_os("HITBOY_PS4_TARGET").expect("Set HITBOY_PS4_TARGET to an unpacked game folder.");
        let mut machine = Machine::load_game(std::path::Path::new(&path)).unwrap();
        let mut calls = 0;
        let mut file_events = 0;
        let mut previous_output = 0;
        let mut captured_failure = false;
        let start = std::time::Instant::now();
        for step in 0..200_000_000 {
            let result = machine.run(1);
            if machine.calls != calls {
                calls = machine.calls;
                if let Some((name, arguments)) = &machine.last_call {
                    if file_events < 40 && matches!(name.as_str(), "sceFiosFHOpenWithModeSync" | "sceKernelOpen" | "open" | "sceKernelLseek" | "lseek") {
                        file_events += 1;
                        println!("File request {name}: args {arguments:x?}; result {:#x}", machine.cpu.r[0]);
                        if name == "sceFiosFHOpenWithModeSync" {
                            for (index, address) in arguments.iter().enumerate() {
                                let text = machine.mem.read_cstr(*address);
                                if text.contains('/') { println!("File path argument {index}: {text}"); }
                                let mut bytes = [0; 16];
                                if machine.mem.read(*address, &mut bytes) { println!("Argument {index} memory: {bytes:02x?}"); }
                            }
                        }
                    }
                    if matches!(name.as_str(), "mmap" | "sceKernelMmap" | "sceKernelMapFlexibleMemory" | "sceKernelMapDirectMemory" | "sceKernelMemoryPoolReserve" | "sceKernelMemoryPoolExpand" | "sceKernelMemoryPoolCommit") && (arguments[1] > (1 << 30) || machine.cpu.r[0] & 0x80000000 != 0) {
                        println!("Memory request {name}: args {arguments:x?}; result {:#x}; errno {}", machine.cpu.r[0], machine.mem.read_u64(machine.cpu.base[FS] + 0x80) as u32);
                    }
                }
            }
            if machine.output.len() != previous_output {
                previous_output = machine.output.len();
                if !captured_failure && machine.output.contains("Failed to allocate virtual address space") {
                    captured_failure = true;
                    println!("Allocation failure registers: {:x?}", machine.cpu.r);
                    let mut frame = machine.cpu.r[5];
                    for depth in 0..16 {
                        let mut bytes = [0; 80];
                        if !machine.mem.read(frame, &mut bytes) { break; }
                        let words = bytes.chunks_exact(8).map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap())).collect::<Vec<_>>();
                        println!("Frame {depth} at {frame:#x}: {words:x?}");
                        if words[0] <= frame || words[0] - frame > 0x10000 { break; }
                        frame = words[0];
                    }
                    let stack = machine.cpu.r[4];
                    for offset in (0..2048).step_by(8) {
                        let value = machine.mem.read_u64(stack + offset);
                        if (0x400000..0x14000000).contains(&value) || (0x40000000..0x40118000).contains(&value) {
                            println!("Stack code candidate at {:#x}: {value:#x}", stack + offset);
                        }
                    }
                }
            }
            if !matches!(result, Ok(Stop::Running)) { println!("Diagnostic stop: {result:?}"); break; }
            if step % 100000 == 0 && start.elapsed().as_secs() >= 120 { break; }
        }
        println!("Guest output: {}", machine.output);
        println!("Diagnostic only: {} calls, unresolved {:?}", machine.calls, machine.unimplemented);
    }

    #[test]
    #[ignore]
    fn real_game_missing_files_do_not_create_a_bogus_allocation_failure() {
        let path = std::env::var_os("HITBOY_PS4_TARGET").expect("Set HITBOY_PS4_TARGET to the private incomplete Fortnite fixture.");
        let mut machine = Machine::load_game(std::path::Path::new(&path)).unwrap();
        let start = std::time::Instant::now();
        let mut result = Ok(Stop::Running);
        for _ in 0..2000 {
            result = machine.run(100000);
            if !matches!(result, Ok(Stop::Running)) || start.elapsed().as_secs() >= 120 { break; }
        }
        assert!(!machine.output.contains("Failed to allocate virtual address space"), "{}", machine.output);
        assert!(!machine.output.contains("PS4 OOM"), "{}", machine.output);
        assert!(machine.output.contains("GEngineLoop.PreInit Failed!"), "{}", machine.output);
        assert_eq!(result.unwrap(), Stop::Exited(0));
        assert!(!machine.unimplemented.iter().any(|name| name == "sceFiosFHOpenWithModeSync"));
        println!("Missing-file regression passed; guest requested exit 0 after reporting PreInit failure. Gameplay remains unverified. Calls: {}. Unresolved: {:?}", machine.calls, machine.unimplemented);
        println!("{}", machine.output);
    }

}
