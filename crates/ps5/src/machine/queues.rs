use super::*;

const BAD_DESCRIPTOR: u64 = 0x8002_0009;
const INVALID: u64 = 0x8002_0016;
const FAULT: u64 = 0x8002_000E;

struct Event {
    ident: u64,
    filter: i16,
    data: u64,
    user: u64,
    pending: bool,
}

#[derive(Default)]
pub(super) struct Queues {
    next: u64,
    entries: HashMap<u64, Vec<Event>>,
}

impl Machine {
    pub(super) fn deliver_equeue(&mut self, id: u64, address: u64, limit: u64, out: u64) -> Result<bool, u64> {
        if limit == 0 || limit > 64 { return Err(INVALID); }
        let mut probe = vec![0u8; limit as usize * 32];
        let mut count_probe = [0u8; 4];
        if !self.mem.read(address, &mut probe) || !self.mem.read(out, &mut count_probe) { return Err(FAULT); }
        let queue = self.queues.entries.get_mut(&id).ok_or(BAD_DESCRIPTOR)?;
        let mut count = 0u32;
        for event in queue.iter_mut().filter(|event| event.pending).take(limit as usize) {
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&event.ident.to_le_bytes());
            bytes[8..10].copy_from_slice(&event.filter.to_le_bytes());
            bytes[10..12].copy_from_slice(&(if event.filter == -14 { 1u16 } else { 0x20 }).to_le_bytes());
            bytes[16..24].copy_from_slice(&event.data.to_le_bytes());
            bytes[24..32].copy_from_slice(&event.user.to_le_bytes());
            self.mem.write(address + count as u64 * 32, &bytes);
            event.pending = false;
            count += 1;
        }
        self.mem.write(out, &count.to_le_bytes());
        Ok(count != 0)
    }

    pub(super) fn signal_video_flip(&mut self, handle: u64) {
        let mut ready = Vec::new();
        for (&id, queue) in &mut self.queues.entries {
            for event in queue.iter_mut().filter(|event| event.filter == -13 && event.ident == handle) {
                event.pending = true;
                event.data = self.video.flips;
                ready.push(id);
            }
        }
        for id in ready { self.wake_equeue_waiters(id); }
    }

    pub(super) fn signal_graphics_completion(&mut self) {
        let mut ready = Vec::new();
        for (&id, queue) in &mut self.queues.entries {
            for event in queue.iter_mut().filter(|event| event.filter == -14 && event.ident == 0x40) {
                event.pending = true;
                event.data = 0x40;
                ready.push(id);
            }
        }
        for id in ready { self.wake_equeue_waiters(id); }
    }

    pub(super) fn call_equeue(&mut self, name: &str) -> bool {
        let id = self.arg(0);
        let result = match name {
            "sceKernelCreateEqueue" => {
                if self.queues.entries.len() >= 1024 { Err(0x8002_0018) } else {
                    let id = self.queues.next + 1;
                    if self.mem.write_u64(self.arg(0), id) {
                        self.queues.next = id;
                        self.queues.entries.insert(id, Vec::new());
                        Ok(0)
                    } else { Err(FAULT) }
                }
            }
            "sceKernelDeleteEqueue" => {
                if self.queues.entries.remove(&id).is_some() {
                    self.wake_equeue_waiters(id);
                    Ok(0)
                } else { Err(BAD_DESCRIPTOR) }
            }
            "sceKernelAddUserEvent" | "sceKernelAddUserEventEdge" | "sceVideoOutAddFlipEvent" | "sceGnmAddEqEvent" => {
                let ident = self.arg(1);
                if name == "sceGnmAddEqEvent" && ident != 0x40 { self.ret(0x8002_004E); return true; }
                let user = if matches!(name, "sceVideoOutAddFlipEvent" | "sceGnmAddEqEvent") { self.arg(2) } else { 0 };
                let filter = match name { "sceVideoOutAddFlipEvent" => -13, "sceGnmAddEqEvent" => -14, _ => -11 };
                match self.queues.entries.get_mut(&id) {
                    Some(queue) if queue.len() >= 1024 => Err(0x8002_0018),
                    Some(queue) => {
                        queue.retain(|event| event.ident != ident || event.filter != filter);
                        queue.push(Event { ident, filter, data: 0, user, pending: false });
                        Ok(0)
                    }
                    None => Err(BAD_DESCRIPTOR),
                }
            }
            "sceKernelDeleteUserEvent" | "sceGnmDeleteEqEvent" => {
                let ident = self.arg(1);
                let filter = if name == "sceGnmDeleteEqEvent" { -14 } else { -11 };
                match self.queues.entries.get_mut(&id) {
                    Some(queue) => {
                        let length = queue.len();
                        queue.retain(|event| event.ident != ident || event.filter != filter);
                        if queue.len() == length { Err(0x8002_0002) } else { Ok(0) }
                    }
                    None => Err(BAD_DESCRIPTOR),
                }
            }
            "sceKernelTriggerUserEvent" => {
                let (ident, user) = (self.arg(1), self.arg(2));
                let event = self.queues.entries.get_mut(&id).and_then(|queue| queue.iter_mut().find(|event| event.ident == ident && event.filter == -11));
                if let Some(event) = event {
                    event.pending = true;
                    event.user = user;
                    event.data = event.data.saturating_add(1);
                    self.wake_equeue_waiters(id);
                    Ok(0)
                } else { Err(0x8002_0002) }
            }
            "sceKernelWaitEqueue" => {
                let (events, count, out, timeout) = (self.arg(1), self.arg(2), self.arg(3), self.arg(4));
                match self.deliver_equeue(id, events, count, out) {
                    Ok(true) => Ok(0),
                    Ok(false) => {
                        let timeout = if timeout == 0 { None } else {
                            let mut bytes = [0u8; 4];
                            if !self.mem.read(timeout, &mut bytes) { self.ret(FAULT); return true; }
                            Some(u32::from_le_bytes(bytes) as u64 * 1000)
                        };
                        self.block_equeue(id, events, count, out, timeout);
                        return true;
                    }
                    Err(error) => Err(error),
                }
            }
            _ => return false,
        };
        self.ret(result.unwrap_or_else(|error| error));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> (Machine, u64) {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        machine.mem.map(0x50000000, 4096, true, false).unwrap();
        machine.cpu.r[7] = 0x50000000;
        assert!(machine.call_equeue("sceKernelCreateEqueue"));
        let id = machine.mem.read_u64(0x50000000);
        (machine, id)
    }

    #[test]
    fn user_events_trigger_deliver_once_and_validate_handles() {
        let (mut machine, id) = machine();
        machine.cpu.r[7] = id;
        machine.cpu.r[6] = 42;
        machine.call_equeue("sceKernelAddUserEvent");
        machine.cpu.r[2] = 123;
        machine.call_equeue("sceKernelTriggerUserEvent");
        assert_eq!(machine.deliver_equeue(id, 0x50000100, 1, 0x50000200), Ok(true));
        assert_eq!(machine.mem.read_u64(0x50000100), 42);
        assert_eq!(machine.mem.read_u64(0x50000118), 123);
        assert_eq!(machine.deliver_equeue(id, 0x50000100, 1, 0x50000200), Ok(false));
        assert_eq!(machine.mem.read_u64(0x50000200), 0);
        assert_eq!(machine.deliver_equeue(id + 1, 0x50000100, 1, 0x50000200), Err(BAD_DESCRIPTOR));
        assert_eq!(machine.deliver_equeue(id, 0x50000100, u64::MAX, 0x50000200), Err(INVALID));
    }

    #[test]
    fn flips_signal_registered_queues_and_empty_poll_times_out() {
        let (mut machine, id) = machine();
        machine.cpu.r[7] = id;
        machine.cpu.r[6] = 1;
        machine.cpu.r[2] = 99;
        machine.call_equeue("sceVideoOutAddFlipEvent");
        machine.video.flips = 7;
        machine.signal_video_flip(1);
        assert_eq!(machine.deliver_equeue(id, 0x50000100, 1, 0x50000200), Ok(true));
        assert_eq!(machine.mem.read_u64(0x50000110), 7);
        assert_eq!(machine.mem.read_u64(0x50000118), 99);
        machine.cpu.r[6] = 0x50000100;
        machine.cpu.r[2] = 1;
        machine.cpu.r[1] = 0x50000200;
        machine.cpu.r[8] = 0x50000300;
        machine.call_equeue("sceKernelWaitEqueue");
        assert_eq!(machine.cpu.r[0], threads::SCE_ETIMEDOUT);
    }

    #[test]
    fn a_blocked_queue_waiter_resumes_on_another_threads_event() {
        let (mut machine, id) = machine();
        machine.cpu.r[7] = id;
        machine.cpu.r[6] = 42;
        machine.call_equeue("sceKernelAddUserEvent");
        assert_eq!(machine.spawn(0x401234, 0, 0x50000300), 0);
        machine.cpu.r[7] = id;
        machine.cpu.r[6] = 0x50000100;
        machine.cpu.r[2] = 1;
        machine.cpu.r[1] = 0x50000200;
        machine.cpu.r[8] = 0;
        machine.call_equeue("sceKernelWaitEqueue");
        assert_eq!(machine.tid(), 2);
        machine.cpu.r[7] = id;
        machine.cpu.r[6] = 42;
        machine.cpu.r[2] = 123;
        machine.call_equeue("sceKernelTriggerUserEvent");
        machine.yield_now();
        assert_eq!(machine.tid(), 1);
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.mem.read_u64(0x50000118), 123);
    }
}
