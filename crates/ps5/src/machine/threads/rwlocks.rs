use super::*;

#[derive(Default)]
pub(super) struct RwLock {
    writer: Option<u64>,
    readers: HashMap<u64, u32>,
}

impl Machine {
    fn rwlock_open(&mut self, address: u64) -> Result<(), u64> {
        let mut bytes = [0u8; 8];
        if address == 0 || !self.mem.read(address, &mut bytes) { return Err(22); }
        if !self.sched.rwlocks.contains_key(&address) {
            if bytes != [0; 8] || !self.mem.write_u64(address, address) { return Err(22); }
            self.sched.rwlocks.insert(address, RwLock::default());
        }
        Ok(())
    }

    fn rwlock_result(&mut self, code: u64, sony: bool) {
        self.ret(if sony && code != 0 { 0x8002_0000 | code } else { code });
    }

    fn rwlock_acquire(&mut self, address: u64, write: bool, wait: bool, sony: bool) {
        if let Err(error) = self.rwlock_open(address) { self.rwlock_result(error, sony); return; }
        let id = self.tid();
        let queued_writer = self.sched.threads.iter().any(|thread| thread.wait == Wait::RwLock { address, write: true });
        let lock = self.sched.rwlocks.get_mut(&address).unwrap();
        let owned_read = lock.readers.contains_key(&id);
        if lock.writer == Some(id) || (write && owned_read) {
            self.rwlock_result(if wait { 11 } else { 16 }, sony);
            return;
        }
        let available = lock.writer.is_none() && if write { lock.readers.is_empty() } else { !queued_writer || owned_read };
        if available {
            if write { lock.writer = Some(id); }
            else {
                let count = lock.readers.entry(id).or_default();
                let Some(next) = count.checked_add(1) else { self.rwlock_result(35, sony); return; };
                *count = next;
            }
            self.ret(0);
        } else if wait { self.block(Wait::RwLock { address, write }, None, 0); }
        else { self.rwlock_result(16, sony); }
    }

    fn rwlock_wake(&mut self, address: u64) {
        let Some(lock) = self.sched.rwlocks.get(&address) else { return; };
        if lock.writer.is_some() || !lock.readers.is_empty() { return; }
        if let Some(index) = self.sched.threads.iter().position(|thread| thread.wait == Wait::RwLock { address, write: true }) {
            let id = self.sched.threads[index].id;
            self.sched.rwlocks.get_mut(&address).unwrap().writer = Some(id);
            self.wake(index, 0);
            return;
        }
        for index in 0..self.sched.threads.len() {
            if self.sched.threads[index].wait == (Wait::RwLock { address, write: false }) {
                let id = self.sched.threads[index].id;
                self.sched.rwlocks.get_mut(&address).unwrap().readers.insert(id, 1);
                self.wake(index, 0);
            }
        }
    }

    fn rwlock_unlock(&mut self, address: u64, sony: bool) {
        let id = self.tid();
        let Some(lock) = self.sched.rwlocks.get_mut(&address) else { self.rwlock_result(22, sony); return; };
        if lock.writer == Some(id) { lock.writer = None; }
        else if let Some(count) = lock.readers.get_mut(&id) {
            *count -= 1;
            if *count == 0 { lock.readers.remove(&id); }
        } else { self.rwlock_result(1, sony); return; }
        self.rwlock_wake(address);
        self.ret(0);
    }

    pub(in crate::machine) fn call_rwlock(&mut self, name: &str) -> bool {
        let (operation, sony) = match name.strip_prefix("scePthreadRwlock") {
            Some(operation) => (operation.to_ascii_lowercase(), true),
            None => match name.strip_prefix("pthread_rwlock_") {
                Some(operation) => (operation.to_string(), false),
                None => return false,
            },
        };
        let address = self.arg(0);
        match operation.as_str() {
            "init" => {
                let busy = self.sched.rwlocks.get(&address).is_some_and(|lock| lock.writer.is_some() || !lock.readers.is_empty());
                let code = if busy { 16 } else if address == 0 || !self.mem.write_u64(address, address) { 22 } else {
                    self.sched.rwlocks.insert(address, RwLock::default());
                    0
                };
                self.rwlock_result(code, sony);
            }
            "destroy" => {
                let waiting = self.sched.threads.iter().any(|thread| matches!(thread.wait, Wait::RwLock { address: key, .. } if key == address));
                let code = match self.sched.rwlocks.get(&address) {
                    None => 22,
                    Some(lock) if waiting || lock.writer.is_some() || !lock.readers.is_empty() => 16,
                    Some(_) => {
                        self.sched.rwlocks.remove(&address);
                        self.mem.write_u64(address, 0);
                        0
                    }
                };
                self.rwlock_result(code, sony);
            }
            "rdlock" => self.rwlock_acquire(address, false, true, sony),
            "wrlock" => self.rwlock_acquire(address, true, true, sony),
            "tryrdlock" => self.rwlock_acquire(address, false, false, sony),
            "trywrlock" => self.rwlock_acquire(address, true, false, sony),
            "unlock" => self.rwlock_unlock(address, sony),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(machine: &mut Machine, operation: &str, address: u64) {
        machine.cpu.r[7] = address;
        assert!(machine.call_rwlock(operation));
    }

    #[test]
    fn writer_waits_for_every_recursive_and_shared_reader() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let address = 0x50000000;
        machine.mem.map(address, 4096, true, false).unwrap();
        call(&mut machine, "pthread_rwlock_init", address);
        call(&mut machine, "pthread_rwlock_rdlock", address);
        call(&mut machine, "pthread_rwlock_rdlock", address);
        assert_eq!(machine.spawn(0x401234, 0, 0), 0);
        machine.yield_now();
        assert_eq!(machine.tid(), 2);
        call(&mut machine, "pthread_rwlock_rdlock", address);
        assert_eq!(machine.spawn(0x401234, 0, 0), 0);
        machine.yield_now();
        assert_eq!(machine.tid(), 3);
        call(&mut machine, "pthread_rwlock_wrlock", address);
        assert_eq!(machine.tid(), 1);
        call(&mut machine, "pthread_rwlock_unlock", address);
        call(&mut machine, "pthread_rwlock_unlock", address);
        assert_eq!(machine.sched.rwlocks[&address].writer, None);
        machine.yield_now();
        assert_eq!(machine.tid(), 2);
        call(&mut machine, "pthread_rwlock_unlock", address);
        assert_eq!(machine.sched.rwlocks[&address].writer, Some(3));
        call(&mut machine, "pthread_rwlock_tryrdlock", address);
        assert_eq!(machine.cpu.r[0], 16);
        machine.yield_now();
        assert_eq!(machine.tid(), 3);
        assert_eq!(machine.cpu.r[0], 0);
        call(&mut machine, "pthread_rwlock_unlock", address);
        call(&mut machine, "pthread_rwlock_destroy", address);
        assert_eq!(machine.cpu.r[0], 0);
    }

    #[test]
    fn releasing_a_writer_wakes_all_waiting_readers_and_self_deadlock_has_the_ps4_error_code() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let address = 0x50000000;
        machine.mem.map(address, 4096, true, false).unwrap();
        call(&mut machine, "pthread_rwlock_wrlock", address);
        call(&mut machine, "scePthreadRwlockWrlock", address);
        assert_eq!(machine.cpu.r[0], 0x8002000B);
        assert_eq!(machine.spawn(0x401234, 0, 0), 0);
        machine.yield_now();
        call(&mut machine, "pthread_rwlock_rdlock", address);
        assert_eq!(machine.tid(), 1);
        assert_eq!(machine.spawn(0x401234, 0, 0), 0);
        machine.yield_now();
        call(&mut machine, "pthread_rwlock_rdlock", address);
        assert_eq!(machine.tid(), 1);
        call(&mut machine, "pthread_rwlock_unlock", address);
        assert_eq!(machine.sched.rwlocks[&address].readers.len(), 2);
        machine.yield_now();
        assert_eq!(machine.tid(), 2);
        assert_eq!(machine.cpu.r[0], 0);
        call(&mut machine, "pthread_rwlock_unlock", address);
        machine.yield_now();
        assert_eq!(machine.tid(), 3);
        assert_eq!(machine.cpu.r[0], 0);
        call(&mut machine, "pthread_rwlock_unlock", address);
        assert!(machine.sched.rwlocks[&address].readers.is_empty());
    }

    #[test]
    fn invalid_ownership_busy_destroy_and_sony_errors_are_reported() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let address = 0x50000000;
        machine.mem.map(address, 4096, true, false).unwrap();
        call(&mut machine, "scePthreadRwlockInit", 0);
        assert_eq!(machine.cpu.r[0], 0x80020016);
        call(&mut machine, "scePthreadRwlockRdlock", address);
        call(&mut machine, "scePthreadRwlockDestroy", address);
        assert_eq!(machine.cpu.r[0], 0x80020010);
        assert_eq!(machine.spawn(0x401234, 0, 0), 0);
        machine.yield_now();
        call(&mut machine, "pthread_rwlock_unlock", address);
        assert_eq!(machine.cpu.r[0], 1);
        call(&mut machine, "pthread_rwlock_trywrlock", address);
        assert_eq!(machine.cpu.r[0], 16);
    }
}
