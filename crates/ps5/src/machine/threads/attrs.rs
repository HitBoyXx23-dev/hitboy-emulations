use super::*;

impl Machine {
    pub(super) fn thread_attr_handle(&self, address: u64) -> Option<u64> {
        let mut bytes = [0u8; 8];
        if !self.mem.read(address, &mut bytes) { return None; }
        let handle = u64::from_le_bytes(bytes);
        self.sched.attributes.contains(&handle).then_some(handle)
    }

    fn thread_attr_init(&mut self, output: u64) -> u64 {
        if output == 0 || !self.mem.read(output, &mut [0u8; 8]) { return 22; }
        if self.sched.attributes.len() >= 1024 { return 12; }
        let handle = self.heap_alloc(16, 16);
        if handle == 0 { return 12; }
        self.mem.write_u64(handle, 1);
        self.mem.write_u64(handle + 8, THREAD_STACK);
        self.mem.write_u64(output, handle);
        self.sched.attributes.insert(handle);
        0
    }

    fn set_thread_scheduling(&mut self) -> u64 {
        let target = self.arg(0);
        if self.index_of(target).is_none_or(|index| self.sched.threads[index].wait == Wait::Done) { return 3; }
        let mut parameters = [0; 4];
        if self.arg(2) == 0 || !self.mem.read(self.arg(2), &mut parameters) { return 14; }
        let priority = i32::from_le_bytes(parameters);
        let policy = self.arg(1);
        if policy > 2 || priority < 0 { return 22; }
        if policy != 0 || priority != 0 { return 45; }
        0
    }

    pub(in crate::machine) fn call_thread_attr(&mut self, name: &str) -> bool {
        let sony = name.starts_with("sce");
        let code = match name {
            "pthread_setschedparam" | "scePthreadSetschedparam" => self.set_thread_scheduling(),
            "scePthreadAttrInit" | "pthread_attr_init" => self.thread_attr_init(self.arg(0)),
            "scePthreadAttrDestroy" | "pthread_attr_destroy" => {
                match self.thread_attr_handle(self.arg(0)) {
                    Some(handle) => {
                        self.sched.attributes.remove(&handle);
                        self.mem.write_u64(self.arg(0), 0);
                        0
                    }
                    None => 22,
                }
            }
            "scePthreadAttrGet" | "pthread_attr_get_np" => {
                if self.index_of(self.arg(0)).is_none_or(|index| self.sched.threads[index].wait == Wait::Done) { 3 }
                else if let Some(handle) = self.thread_attr_handle(self.arg(1)) {
                    self.mem.write_u64(handle, 1);
                    0
                } else { 22 }
            }
            "pthread_attr_setstacksize" | "scePthreadAttrSetstacksize" => {
                match self.thread_attr_handle(self.arg(0)) {
                    Some(handle) if (0x4000..=0x1000000).contains(&self.arg(1)) => {
                        if self.mem.write_u64(handle + 8, self.arg(1)) { 0 } else { 14 }
                    }
                    _ => 22,
                }
            }
            "pthread_attr_getstacksize" | "scePthreadAttrGetstacksize" => {
                match self.thread_attr_handle(self.arg(0)) {
                    Some(handle) if self.arg(1) != 0 && self.mem.write_u64(self.arg(1), self.mem.read_u64(handle + 8)) => 0,
                    Some(_) => 14,
                    None => 22,
                }
            }
            "scePthreadAttrGetaffinity" => {
                match self.thread_attr_handle(self.arg(0)) {
                    Some(handle) if self.mem.write_u64(self.arg(1), self.mem.read_u64(handle)) => 0,
                    Some(_) => 14,
                    None => 22,
                }
            }
            _ => return false,
        };
        self.ret(if sony && code != 0 { 0x8002_0000 | code } else { code });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialized_thread_attributes_report_the_single_virtual_cpu_and_validate_handles() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let attribute = 0x50000000;
        let output = attribute + 0x100;
        machine.mem.map(attribute, 4096, true, false).unwrap();
        machine.cpu.r[7] = attribute;
        machine.call_thread_attr("scePthreadAttrInit");
        assert_eq!(machine.cpu.r[0], 0);
        machine.cpu.r[7] = machine.tid();
        machine.cpu.r[6] = attribute;
        machine.call_thread_attr("scePthreadAttrGet");
        assert_eq!(machine.cpu.r[0], 0);
        machine.cpu.r[7] = attribute;
        machine.cpu.r[6] = output;
        machine.mem.write_u64(output + 8, u64::MAX);
        machine.call_thread_attr("scePthreadAttrGetaffinity");
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.mem.read_u64(output), 1);
        assert_eq!(machine.mem.read_u64(output + 8), u64::MAX);
        machine.call_thread_attr("scePthreadAttrDestroy");
        assert_eq!(machine.mem.read_u64(attribute), 0);
        machine.call_thread_attr("scePthreadAttrGetaffinity");
        assert_eq!(machine.cpu.r[0], 0x80020016);
        machine.cpu.r[7] = 99;
        machine.cpu.r[6] = attribute;
        machine.call_thread_attr("scePthreadAttrGet");
        assert_eq!(machine.cpu.r[0], 0x80020003);
    }
}
