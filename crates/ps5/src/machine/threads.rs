//! Cooperative threads for the PlayStation machine.
//!
//! The CPU core is single, so threads take turns, and only switch inside a system call (a stub
//! trap): at that moment the program is in the middle of a function call, so everything the
//! System V calling convention lets the callee clobber is dead and a thread's saved state is just
//! its registers, instruction pointer and thread-local-storage base. A thread gives up the CPU
//! when it waits (join, locked mutex, condition, semaphore, sleep, yield) or finishes.

use super::*;

mod rwlocks;
mod attrs;

/// Where threads' thread-local blocks and stacks are placed.
const THREAD_REGION: u64 = 0x8_0000_0000;
const THREAD_STEP: u64 = 0x20_0000;
const TLS_AREA: u64 = 0x1_0000;
const THREAD_STACK: u64 = 1 << 20;

pub const SCE_ETIMEDOUT: u64 = 0x8002_003C;

#[derive(Clone, Copy, Default)]
struct Ctx {
    r: [u64; 16],
    rip: u64,
    fs: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Wait {
    Ready,
    Sleep,
    Join(u64),
    Mutex(u64),
    RwLock { address: u64, write: bool },
    Cond { cond: u64, mutex: u64 },
    Sema { id: u64, need: u64 },
    Equeue { id: u64, events: u64, count: u64, out: u64 },
    Done,
}

struct Thread {
    id: u64,
    name: String,
    ctx: Ctx,
    wait: Wait,
    deadline: Option<u64>,
    /// Value the thread's blocked call returns if it is woken by the deadline.
    timeout_code: u64,
    /// Value handed back once a condition waiter finally owns its mutex again.
    pending: u64,
    exit_value: u64,
    cleanup_started: bool,
    join_out: u64,
}

pub(super) struct Sched {
    threads: Vec<Thread>,
    cur: usize,
    mutexes: HashMap<u64, (u64, u32)>,
    semas: HashMap<u64, (u64, u64)>,
    rwlocks: HashMap<u64, rwlocks::RwLock>,
    attributes: std::collections::HashSet<u64>,
    next_id: u64,
    next_region: u64,
    tls_image: Vec<u8>,
    tls_size: u64,
    exit_stub: u64,
    pub(super) thread_dtor: u64,
    /// Set when nothing can run any more.
    pub(super) stuck: Option<String>,
}

impl Sched {
    pub(super) fn new(tls_image: Vec<u8>, tls_size: u64, exit_stub: u64) -> Sched {
        let main = Thread { id: 1, name: "main".into(), ctx: Ctx::default(), wait: Wait::Ready, deadline: None, timeout_code: 0, pending: 0, exit_value: 0, cleanup_started: false, join_out: 0 };
        Sched { threads: vec![main], cur: 0, mutexes: HashMap::new(), semas: HashMap::new(), rwlocks: HashMap::new(), attributes: std::collections::HashSet::new(), next_id: 2, next_region: THREAD_REGION, tls_image, tls_size, exit_stub, thread_dtor: 0, stuck: None }
    }

    pub(super) fn live_threads(&self) -> usize {
        self.threads.iter().filter(|t| t.wait != Wait::Done).count()
    }
}

impl Machine {
    pub(super) fn tid(&self) -> u64 {
        self.sched.threads[self.sched.cur].id
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.sched.threads.iter().position(|t| t.id == id)
    }

    fn wake(&mut self, i: usize, value: u64) {
        let t = &mut self.sched.threads[i];
        t.wait = Wait::Ready;
        t.deadline = None;
        t.ctx.r[0] = value;
    }

    /// Gives the CPU to another thread (or keeps it if none else can run).
    pub(super) fn switch(&mut self) {
        let cur = self.sched.cur;
        if self.sched.threads[cur].wait != Wait::Done {
            self.sched.threads[cur].ctx = Ctx { r: self.cpu.r, rip: self.cpu.rip, fs: self.cpu.base[FS] };
        }
        loop {
            // Threads whose wait has timed out.
            let now = self.cpu.tsc;
            for i in 0..self.sched.threads.len() {
                let (wait, due, code) = {
                    let t = &self.sched.threads[i];
                    (t.wait, t.deadline.is_some_and(|d| d <= now), t.timeout_code)
                };
                if !due || wait == Wait::Ready || wait == Wait::Done {
                    continue;
                }
                match wait {
                    Wait::Cond { mutex, .. } => {
                        // The waiter must own its mutex again before it runs.
                        if self.sched.mutexes.contains_key(&mutex) {
                            let t = &mut self.sched.threads[i];
                            t.wait = Wait::Mutex(mutex);
                            t.deadline = None;
                            t.pending = code;
                        } else {
                            let id = self.sched.threads[i].id;
                            self.sched.mutexes.insert(mutex, (id, 1));
                            self.wake(i, code);
                        }
                    }
                    Wait::Equeue { out, .. } => {
                        self.put32(out, 0);
                        self.wake(i, code);
                    }
                    Wait::Sleep => self.wake(i, 0),
                    _ => self.wake(i, code),
                }
            }
            let n = self.sched.threads.len();
            if let Some(next) = (1..=n).map(|k| (cur + k) % n).find(|&i| self.sched.threads[i].wait == Wait::Ready) {
                let c = self.sched.threads[next].ctx;
                self.sched.cur = next;
                self.cpu.r = c.r;
                self.cpu.rip = c.rip;
                self.cpu.base[FS] = c.fs;
                return;
            }
            match self.sched.threads.iter().filter_map(|t| t.deadline).min() {
                Some(d) => self.cpu.tsc = self.cpu.tsc.max(d),
                None => {
                    self.sched.stuck = Some("every thread is waiting for something that can never happen (a deadlock)".into());
                    return;
                }
            }
        }
    }

    fn block(&mut self, wait: Wait, deadline: Option<u64>, timeout_code: u64) {
        let t = &mut self.sched.threads[self.sched.cur];
        t.wait = wait;
        t.deadline = deadline;
        t.timeout_code = timeout_code;
        self.switch();
    }

    pub(super) fn block_equeue(&mut self, id: u64, events: u64, count: u64, out: u64, timeout: Option<u64>) {
        if timeout == Some(0) {
            self.ret(SCE_ETIMEDOUT);
            return;
        }
        let deadline = timeout.map(|duration| self.cpu.tsc.saturating_add(duration));
        self.block(Wait::Equeue { id, events, count, out }, deadline, SCE_ETIMEDOUT);
    }

    pub(super) fn wake_equeue_waiters(&mut self, id: u64) {
        for index in 0..self.sched.threads.len() {
            let Wait::Equeue { id: queue, events, count, out } = self.sched.threads[index].wait else { continue };
            if queue != id { continue; }
            match self.deliver_equeue(id, events, count, out) {
                Ok(true) => self.wake(index, 0),
                Ok(false) => {}
                Err(error) => self.wake(index, error),
            }
        }
    }

    pub(super) fn sleep_ns(&mut self, ns: u64) {
        self.ret(0);
        let until = self.cpu.tsc.saturating_add(ns);
        self.block(Wait::Sleep, Some(until), 0);
    }

    pub(super) fn yield_now(&mut self) {
        self.ret(0);
        self.switch();
    }

    // ---------------------------------------------------------------- threads

    pub(super) fn spawn(&mut self, start: u64, arg: u64, out: u64) -> u64 {
        self.spawn_sized(start, arg, out, THREAD_STACK)
    }

    fn spawn_sized(&mut self, start: u64, arg: u64, out: u64, stack_size: u64) -> u64 {
        const EAGAIN: u64 = 35;
        if self.sched.tls_size + 0x1000 > TLS_AREA {
            return EAGAIN;
        }
        let region = self.sched.next_region;
        if self.mem.map(region, TLS_AREA, true, false).is_err() || self.mem.map(region + TLS_AREA, stack_size, true, false).is_err() {
            return EAGAIN;
        }
        self.sched.next_region += (TLS_AREA + stack_size + 0xFFFF + THREAD_STEP - 1) & !(THREAD_STEP - 1);
        let image = self.sched.tls_image.clone();
        self.mem.write(region, &image);
        let fs = region + self.sched.tls_size;
        self.mem.write_u64(fs, fs);
        self.mem.write_u64(fs + 0x28, 0x5A5A_5A5A_1234_5678);
        let rsp = region + TLS_AREA + stack_size - 0x100 - 8;
        self.mem.write_u64(rsp, self.sched.exit_stub);
        let id = self.sched.next_id;
        self.sched.next_id += 1;
        let mut ctx = Ctx { rip: start, fs, ..Ctx::default() };
        ctx.r[4] = rsp;
        ctx.r[7] = arg;
        self.sched.threads.push(Thread { id, name: String::new(), ctx, wait: Wait::Ready, deadline: None, timeout_code: 0, pending: 0, exit_value: 0, cleanup_started: false, join_out: 0 });
        if out != 0 {
            self.mem.write_u64(out, id);
        }
        0
    }

    pub(super) fn create_thread(&mut self, named: bool) -> u64 {
        if self.sched.tls_size + 0x1000 > TLS_AREA { return 35; }
        let output = self.arg(0);
        let attributes = self.arg(1);
        let start = self.arg(2);
        let argument = self.arg(3);
        if output == 0 || !self.mem.read(output, &mut [0; 8]) { return 14; }
        if start == 0 || self.mem.phys(start).is_none() { return 22; }
        let stack_size = if attributes == 0 { THREAD_STACK } else {
            let Some(handle) = self.thread_attr_handle(attributes) else { return 22; };
            self.mem.read_u64(handle + 8)
        };
        if !(0x4000..=0x1000000).contains(&stack_size) { return 22; }
        let stack_size = (stack_size + 0x3FFF) & !0x3FFF;
        let mut name = Vec::new();
        if named && self.arg(4) != 0 {
            for index in 0..64 {
                let mut byte = [0];
                if !self.arg(4).checked_add(index).is_some_and(|address| self.mem.read(address, &mut byte)) { return 14; }
                if byte[0] == 0 { break; }
                name.push(byte[0]);
            }
            if name.len() >= 64 { return 34; }
        }
        let Ok(name) = String::from_utf8(name) else { return 22; };
        let result = if stack_size == THREAD_STACK { self.spawn(start, argument, output) } else { self.spawn_sized(start, argument, output, stack_size) };
        if result == 0 { self.sched.threads.last_mut().unwrap().name = name; }
        result
    }

    pub fn thread_report(&self) -> String {
        let states = self.sched.threads.iter().map(|thread| format!("{}:{:?}:{:?}", thread.id, thread.name, thread.wait)).collect::<Vec<_>>();
        format!("threads={}", states.join("; "))
    }

    /// The current thread ends with `value`.
    pub(super) fn thread_exit(&mut self, value: u64) {
        let me = self.sched.cur;
        if !self.sched.threads[me].cleanup_started {
            self.sched.threads[me].cleanup_started = true;
            self.sched.threads[me].exit_value = value;
            if self.sched.thread_dtor != 0 {
                let Some(stack) = (self.cpu.r[4] & !15).checked_sub(8) else {
                    self.runtime_error = Some("The thread cleanup stack is invalid.".into());
                    return;
                };
                if !self.mem.write_u64(stack, self.sched.exit_stub) {
                    self.runtime_error = Some("The thread cleanup stack is not mapped.".into());
                    return;
                }
                self.cpu.r[4] = stack;
                self.cpu.rip = self.sched.thread_dtor;
                return;
            }
        }
        let value = self.sched.threads[me].exit_value;
        let id = self.sched.threads[me].id;
        self.sched.threads[me].wait = Wait::Done;
        for i in 0..self.sched.threads.len() {
            if self.sched.threads[i].wait == Wait::Join(id) {
                let out = self.sched.threads[i].join_out;
                if out != 0 {
                    self.mem.write_u64(out, value);
                }
                self.wake(i, 0);
            }
        }
        if self.sched.live_threads() == 0 { self.exit_code = Some(0); }
        else { self.switch(); }
    }

    pub(super) fn join(&mut self, target: u64, out: u64) {
        match self.index_of(target) {
            None => self.ret(3), // ESRCH
            Some(i) if self.sched.threads[i].wait == Wait::Done => {
                if out != 0 {
                    let v = self.sched.threads[i].exit_value;
                    self.mem.write_u64(out, v);
                }
                self.ret(0);
            }
            Some(_) => {
                self.sched.threads[self.sched.cur].join_out = out;
                self.block(Wait::Join(target), None, 0);
            }
        }
    }

    // ---------------------------------------------------------------- mutexes

    pub(super) fn mutex_is_locked(&self, address: u64) -> bool {
        self.sched.mutexes.contains_key(&address)
    }

    pub(super) fn mutex_lock(&mut self, addr: u64, wait: bool) {
        let me = self.tid();
        match self.sched.mutexes.get_mut(&addr) {
            None => {
                self.sched.mutexes.insert(addr, (me, 1));
                self.ret(0);
            }
            Some((owner, count)) if *owner == me => {
                *count += 1;
                self.ret(0);
            }
            Some(_) if !wait => self.ret(16), // EBUSY
            Some(_) => self.block(Wait::Mutex(addr), None, 0),
        }
    }

    /// Releases `addr` and hands it to the first thread waiting for it.
    fn release(&mut self, addr: u64) {
        let me = self.tid();
        match self.sched.mutexes.get_mut(&addr) {
            Some((owner, count)) if *owner == me && *count > 1 => *count -= 1,
            Some((owner, _)) if *owner == me => {
                self.sched.mutexes.remove(&addr);
                if let Some(i) = (0..self.sched.threads.len()).find(|&i| self.sched.threads[i].wait == Wait::Mutex(addr)) {
                    let id = self.sched.threads[i].id;
                    let code = std::mem::take(&mut self.sched.threads[i].pending);
                    self.sched.mutexes.insert(addr, (id, 1));
                    self.wake(i, code);
                }
            }
            _ => {}
        }
    }

    pub(super) fn mutex_unlock(&mut self, addr: u64) {
        self.release(addr);
        self.ret(0);
    }

    pub(super) fn mutex_destroy(&mut self, addr: u64) {
        self.sched.mutexes.remove(&addr);
        self.ret(0);
    }

    // ---------------------------------------------------------------- conditions

    pub(super) fn cond_wait(&mut self, cond: u64, mutex: u64, timeout_ns: Option<u64>) {
        self.release(mutex);
        let deadline = timeout_ns.map(|t| self.cpu.tsc.saturating_add(t));
        self.block(Wait::Cond { cond, mutex }, deadline, SCE_ETIMEDOUT);
    }

    pub(super) fn posix_cond_timedwait(&mut self) {
        let (condition, mutex, timeout) = (self.arg(0), self.arg(1), self.arg(2));
        let mut timespec = [0; 16];
        if timeout == 0 || !self.mem.read(timeout, &mut timespec) { self.ret(14); return; }
        let seconds = i64::from_le_bytes(timespec[..8].try_into().unwrap());
        let nanoseconds = i64::from_le_bytes(timespec[8..].try_into().unwrap());
        if seconds < 0 || !(0..1000000000).contains(&nanoseconds) { self.ret(22); return; }
        let Some(absolute) = (seconds as u64).checked_mul(1000000000).and_then(|value| value.checked_add(nanoseconds as u64)) else { self.ret(22); return; };
        if condition == 0 || self.mem.phys(condition).is_none() { self.ret(22); return; }
        if self.sched.mutexes.get(&mutex).is_none_or(|(owner, _)| *owner != self.tid()) { self.ret(1); return; }
        let deadline = absolute.saturating_sub(1700000000 * 1000000000);
        self.release(mutex);
        self.block(Wait::Cond { cond: condition, mutex }, Some(deadline), 60);
    }

    pub(super) fn cond_signal(&mut self, cond: u64, all: bool) {
        for i in 0..self.sched.threads.len() {
            let Wait::Cond { cond: c, mutex } = self.sched.threads[i].wait else { continue };
            if c != cond {
                continue;
            }
            if self.sched.mutexes.contains_key(&mutex) {
                let t = &mut self.sched.threads[i];
                t.wait = Wait::Mutex(mutex);
                t.deadline = None;
            } else {
                let id = self.sched.threads[i].id;
                self.sched.mutexes.insert(mutex, (id, 1));
                self.wake(i, 0);
            }
            if !all {
                break;
            }
        }
        self.ret(0);
    }

    // ---------------------------------------------------------------- semaphores

    pub(super) fn sema_create(&mut self, out: u64, init: u64, max: u64) {
        let id = self.sched.next_id;
        self.sched.next_id += 1;
        self.sched.semas.insert(id, (init, max));
        self.put32(out, id as u32);
        self.ret(0);
    }

    pub(super) fn sema_wait(&mut self, id: u64, need: u64, timeout_us: Option<u64>) {
        match self.sched.semas.get_mut(&id) {
            None => self.ret(0x8002_0016), // EINVAL
            Some((count, _)) if *count >= need => {
                *count -= need;
                self.ret(0);
            }
            Some(_) => {
                let deadline = timeout_us.map(|us| self.cpu.tsc.saturating_add(us * 1000));
                self.block(Wait::Sema { id, need }, deadline, SCE_ETIMEDOUT);
            }
        }
    }

    pub(super) fn sema_signal(&mut self, id: u64, n: u64) {
        let Some((count, max)) = self.sched.semas.get_mut(&id) else {
            return self.ret(0x8002_0016);
        };
        *count = (*count + n).min(*max);
        for i in 0..self.sched.threads.len() {
            if let Wait::Sema { id: sid, need } = self.sched.threads[i].wait {
                if sid == id {
                    let (count, _) = self.sched.semas.get_mut(&id).unwrap();
                    if *count >= need {
                        *count -= need;
                        self.wake(i, 0);
                    }
                }
            }
        }
        self.ret(0);
    }

    pub(super) fn sema_delete(&mut self, id: u64) {
        self.sched.semas.remove(&id);
        self.ret(0);
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;

    #[test]
    fn posix_condition_timeouts_reacquire_mutexes_and_real_signals_beat_deadlines() {
        let setup = || {
            let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
            let storage = machine.heap_alloc(64, 16);
            let (condition, mutex, timeout) = (storage + 32, storage + 40, storage);
            machine.mem.write_u64(timeout, 1700000000);
            machine.mem.write_u64(timeout + 8, 5000);
            machine.mutex_lock(mutex, true);
            machine.cpu.r[7] = condition;
            machine.cpu.r[6] = mutex;
            machine.cpu.r[2] = timeout;
            (machine, condition, mutex, timeout)
        };
        let (mut machine, _, mutex, timeout) = setup();
        machine.posix_cond_timedwait();
        assert_eq!(machine.cpu.r[0], 60);
        assert_eq!(machine.cpu.tsc, 5000);
        assert_eq!(machine.sched.mutexes.get(&mutex), Some(&(1, 1)));
        machine.mem.write_u64(timeout + 8, 1000000000);
        machine.posix_cond_timedwait();
        assert_eq!(machine.cpu.r[0], 22);
        assert_eq!(machine.sched.mutexes.get(&mutex), Some(&(1, 1)));
        let (mut machine, condition, mutex, _) = setup();
        assert_eq!(machine.spawn(machine.entry, 0, 0), 0);
        machine.posix_cond_timedwait();
        assert_eq!(machine.tid(), 2);
        machine.mutex_lock(mutex, true);
        machine.cond_signal(condition, false);
        machine.mutex_unlock(mutex);
        machine.yield_now();
        assert_eq!(machine.tid(), 1);
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.sched.mutexes.get(&mutex), Some(&(1, 1)));
        assert!(machine.cpu.tsc < 5000);
        assert!(machine.sched.stuck.is_none());
    }

    #[test]
    fn named_workers_execute_with_the_requested_stack_and_join_result() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let code = 0x50000000;
        machine.mem.map(code, 4096, true, true).unwrap();
        machine.mem.write(code, &[0x48, 0xC7, 0x07, 42, 0, 0, 0, 0x31, 0xC0, 0xC3]);
        let storage = machine.heap_alloc(256, 16);
        machine.mem.write(storage, &[0xCC; 256]);
        machine.mem.write(storage + 128, b"asset-worker\0");
        machine.cpu.r[7] = storage;
        machine.call_thread_attr("pthread_attr_init");
        machine.cpu.r[6] = 0x30000;
        machine.call_thread_attr("pthread_attr_setstacksize");
        assert_eq!(machine.cpu.r[0], 0);
        machine.cpu.r[6] = storage + 80;
        machine.call_thread_attr("pthread_attr_getstacksize");
        assert_eq!(machine.mem.read_u64(storage + 80), 0x30000);
        machine.cpu.r[7] = storage + 16;
        machine.cpu.r[6] = storage;
        machine.cpu.r[2] = code;
        machine.cpu.r[1] = storage + 32;
        machine.cpu.r[8] = storage + 128;
        machine.imports.push("pthread_create_name_np".into());
        machine.call_system(machine.imports.len() - 1);
        assert_eq!(machine.cpu.r[0], 0);
        let worker = machine.mem.read_u64(storage + 16);
        assert_eq!(machine.sched.threads.last().unwrap().name, "asset-worker");
        assert_eq!(machine.sched.threads.last().unwrap().ctx.r[4], THREAD_REGION + TLS_AREA + 0x30000 - 0x108);
        assert_eq!(machine.mem.read_u64(storage + 24), 0xCCCCCCCCCCCCCCCC);
        machine.join(worker, storage + 48);
        assert_eq!(machine.tid(), worker);
        let result = machine.run(1000).unwrap();
        assert_eq!(result, Stop::Exited(7));
        assert_eq!(machine.mem.read_u64(storage + 32), 42);
        assert_eq!(machine.mem.read_u64(storage + 48), 0);
        assert!(machine.sched.stuck.is_none());
    }

    #[test]
    fn thread_creation_failures_use_posix_or_sony_error_encoding() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        machine.sched.tls_size = TLS_AREA;
        for (name, expected) in [("pthread_create", 35), ("scePthreadCreate", 0x80020023)] {
            machine.imports.push(name.into());
            machine.call_system(machine.imports.len() - 1);
            assert_eq!(machine.cpu.r[0], expected);
            assert_eq!(machine.sched.live_threads(), 1);
        }
    }

    #[test]
    fn the_last_exiting_thread_ends_the_process_without_a_deadlock() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        machine.thread_exit(77);
        assert_eq!(machine.run(1).unwrap(), Stop::Exited(0));
        assert!(machine.sched.stuck.is_none());
    }

    #[test]
    fn guest_cleanup_runs_once_and_join_keeps_the_original_return_value() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let code = 0x50000000;
        let cleanup = code + 0x100;
        let counter = code + 0x200;
        let result = code + 0x300;
        machine.mem.map(code, 4096, true, true).unwrap();
        machine.mem.write(code, &[0xB8, 77, 0, 0, 0, 0xC3]);
        let mut callback = vec![0x48, 0xB8];
        callback.extend_from_slice(&counter.to_le_bytes());
        callback.extend_from_slice(&[0x48, 0xFF, 0, 0xB8, 1, 0, 0, 0, 0xC3]);
        machine.mem.write(cleanup, &callback);
        machine.cpu.r[7] = cleanup;
        machine.imports.push("_sceKernelSetThreadDtors".into());
        machine.call_system(machine.imports.len() - 1);
        assert!(machine.unimplemented.is_empty());
        assert_eq!(machine.spawn(code, 0, 0), 0);
        machine.join(2, result);
        assert_eq!(machine.run(10000).unwrap(), Stop::Exited(7));
        assert_eq!(machine.mem.read_u64(counter), 1);
        assert_eq!(machine.mem.read_u64(result), 77);
    }
    #[test]
    fn posix_microsecond_sleep_runs_other_threads_and_reports_their_ids() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let code = 0x50000000;
        machine.mem.map(code, 4096, true, true).unwrap();
        machine.mem.write(code, &[0xB8, 77, 0, 0, 0, 0xC3]);
        assert_eq!(machine.spawn(code, 0, 0), 0);
        machine.cpu.r[7] = (1u64 << 32) | 250;
        machine.imports.push("usleep".into());
        machine.call_system(machine.imports.len() - 1);
        assert_eq!(machine.tid(), 2);
        machine.imports.push("scePthreadGetthreadid".into());
        machine.call_system(machine.imports.len() - 1);
        assert_eq!(machine.cpu.r[0], 2);
        assert_eq!(machine.run(10000).unwrap(), Stop::Exited(7));
        assert!(machine.cpu.tsc >= 250000 && machine.cpu.tsc < 1000000);
        assert!(machine.unimplemented.is_empty());
        assert_eq!(crate::nid::lookup("QcteRwbsnV0"), Some("usleep"));
        assert_eq!(crate::nid::lookup("EI-5-jlq2dE"), Some("scePthreadGetthreadid"));
    }

}
