//! Our own implementation of the Xbox 360 system calls titles import from xboxkrnl.exe and
//! xam.xex. Arguments arrive in r3-r10 and the result goes back in r3.

use crate::machine::{Machine, ThreadState, Wait};

pub const STATUS_SUCCESS: u32 = 0;
pub const STATUS_TIMEOUT: u32 = 0x102;
pub const STATUS_INVALID_HANDLE: u32 = 0xC000_0008;
pub const STATUS_INVALID_PARAMETER: u32 = 0xC000_000D;
pub const STATUS_NO_MEMORY: u32 = 0xC000_0017;
pub const STATUS_NOT_FOUND: u32 = 0xC000_0225;
pub const STATUS_OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
const ERROR_DEVICE_NOT_CONNECTED: u32 = 0x48F;
const ERROR_NO_SUCH_USER: u32 = 0x525;

// Dispatcher object types (DISPATCHER_HEADER.type).
const EVENT_NOTIFICATION: u8 = 0;
const EVENT_SYNC: u8 = 1;
const MUTANT: u8 = 2;
const SEMAPHORE: u8 = 5;
const THREAD: u8 = 6;

impl Machine {
    fn arg(&self, i: usize) -> u32 {
        self.cpu.regs().r[3 + i] as u32
    }
    fn arg64(&self, i: usize) -> u64 {
        self.cpu.regs().r[3 + i]
    }
    fn ret(&mut self, v: u32) {
        let mut r = self.cpu.regs();
        r.r[3] = v as u64;
        self.cpu.set_regs(r);
    }
    fn ret64(&mut self, v: u64) {
        let mut r = self.cpu.regs();
        r.r[3] = v;
        self.cpu.set_regs(r);
    }
    fn kthread(&self) -> u32 {
        self.threads[self.current].kthread
    }

    /// A kernel variable export, created on first use.
    pub(crate) fn variable(&mut self, module: &'static str, ordinal: u16, name: &'static str) -> u32 {
        if let Some(&a) = self.variables.get(&(module, ordinal)) {
            return a;
        }
        let a = self.alloc_pool(0x400);
        match name {
            "XboxHardwareInfo" => {
                self.w32(a, 0x0000_0020);
                self.w8(a + 4, 6); // six hardware threads
            }
            "XboxKrnlVersion" | "XboxKrnlBaseVersion" => {
                self.w16(a, 2);
                self.w16(a + 2, 0);
                self.w16(a + 4, 17559);
                self.w16(a + 6, 0);
            }
            "XexExecutableModuleHandle" => {
                let ldr = self.ldr_entry;
                self.w32(a, ldr);
            }
            "ExLoadedCommandLine" => self.write_bytes(a, b"\"default.xex\"\0"),
            "ExConsoleGameRegion" => self.w32(a, 0xFFFF_FFFF),
            "VdGpuClockInMHz" => self.w32(a, 500),
            "VdHSIOCalibrationLock" => {
                self.w8(a, EVENT_SYNC);
                self.w32(a + 0x10, u32::MAX);
            }
            _ => {}
        }
        self.variables.insert((module, ordinal), a);
        a
    }

    fn update_time_bundle(&mut self) {
        let Some(&(ord, _, _)) = crate::exports::XBOXKRNL.iter().find(|e| e.1 == "KeTimeStampBundle") else { return };
        if let Some(&a) = self.variables.get(&("xboxkrnl.exe", ord)) {
            let (now, sys) = (self.now(), self.system_time());
            self.w64(a, now);
            self.w64(a + 8, sys);
            self.w32(a + 0x10, (now / 10_000) as u32);
        }
    }

    pub(crate) fn syscall(&mut self, idx: usize) -> Result<(), String> {
        let Some(t) = self.thunks.get(idx) else { return Err(format!("Bad kernel call slot {idx}")) };
        let name = t.name;
        self.update_time_bundle();
        if name == "ExitThreadReturn" {
            let code = self.arg(0);
            self.exit_thread(code);
            return Ok(());
        }
        if !self.call(name)? {
            *self.unimplemented.entry(name).or_insert(0) += 1;
            if self.unimplemented[name] == 1 {
                self.log.push(format!("unimplemented: {name}"));
            }
            self.ret(STATUS_SUCCESS);
        }
        Ok(())
    }

    /// Handles one kernel call. Returns false if `name` isn't implemented.
    fn call(&mut self, name: &str) -> Result<bool, String> {
        match name {
            // ---------------------------------------------------------- memory
            "NtAllocateVirtualMemory" => {
                let (base_ptr, size_ptr) = (self.arg(0), self.arg(1));
                let (base, size) = (self.r32(base_ptr), self.r32(size_ptr));
                if base != 0 && self.is_mapped(base) {
                    self.ret(STATUS_SUCCESS);
                } else {
                    let a = self.alloc_virtual(size);
                    if a == 0 {
                        self.ret(STATUS_NO_MEMORY);
                    } else {
                        self.w32(base_ptr, a);
                        self.w32(size_ptr, size.div_ceil(0x1_0000) * 0x1_0000);
                        self.ret(STATUS_SUCCESS);
                    }
                }
            }
            "NtFreeVirtualMemory" | "NtProtectVirtualMemory" | "MmFreePhysicalMemory" | "ExFreePool" | "XamFree" | "MmSetAddressProtect" => self.ret(STATUS_SUCCESS),
            "NtQueryVirtualMemory" => {
                let (addr, info) = (self.arg(0), self.arg(1));
                match self.regions.iter().copied().find(|&(b, s)| addr >= b && addr < b + s) {
                    Some((b, s)) => {
                        let page = addr & !0xFFF;
                        for (i, v) in [page, b, 4, b + s - page, 0x1000, 4, 0x20000].into_iter().enumerate() {
                            self.w32(info + i as u32 * 4, v);
                        }
                        self.ret(STATUS_SUCCESS);
                    }
                    None => self.ret(STATUS_INVALID_PARAMETER),
                }
            }
            "MmQueryAddressProtect" => self.ret(4),
            "MmAllocatePhysicalMemoryEx" => {
                let (size, protect, align) = (self.arg(1), self.arg(2), self.arg(5));
                let a = self.alloc_physical(size, align, protect);
                self.ret(a);
            }
            "MmAllocatePhysicalMemory" => {
                let (size, protect) = (self.arg(1), self.arg(2));
                let a = self.alloc_physical(size, 0, protect);
                self.ret(a);
            }
            "MmGetPhysicalAddress" => {
                let a = self.arg(0);
                self.ret(a & 0x1FFF_FFFF);
            }
            "MmQueryStatistics" => {
                let p = self.arg(0);
                let total = (crate::machine::PHYS_SIZE / 0x1000) as u32;
                let used = self.phys_next / 0x1000;
                self.w32(p + 4, total);
                self.w32(p + 8, 0x1_0000);
                self.w32(p + 12, total.saturating_sub(used));
                self.ret(STATUS_SUCCESS);
            }
            "ExAllocatePool" | "ExAllocatePoolWithTag" | "ExAllocatePoolTypeWithTag" => {
                let a = self.alloc_pool(self.arg(0));
                self.ret(a);
            }
            "XamAlloc" => {
                let (size, out) = (self.arg(1), self.arg(2));
                let a = self.alloc_pool(size);
                self.w32(out, a);
                self.ret(STATUS_SUCCESS);
            }

            // ---------------------------------------------------------- critical sections
            "RtlInitializeCriticalSection" | "RtlInitializeCriticalSectionAndSpinCount" => {
                let cs = self.arg(0);
                self.w8(cs, EVENT_SYNC);
                self.w32(cs + 4, 0);
                self.w32(cs + 0x10, u32::MAX);
                self.w32(cs + 0x14, 0);
                self.w32(cs + 0x18, 0);
                self.ret(STATUS_SUCCESS);
            }
            "RtlEnterCriticalSection" | "RtlTryEnterCriticalSection" => {
                let cs = self.arg(0);
                let me = self.kthread();
                let owner = self.r32(cs + 0x18);
                if owner == 0 || owner == me {
                    self.w32(cs + 0x18, me);
                    let rec = self.r32(cs + 0x14);
                    self.w32(cs + 0x14, rec + 1);
                    let lock = self.r32(cs + 0x10);
                    self.w32(cs + 0x10, lock.wrapping_add(1));
                    self.threads[self.current].wait = None;
                    self.ret(1);
                } else if name == "RtlTryEnterCriticalSection" {
                    self.ret(0);
                } else {
                    // Blocks until the owner leaves; `cs` is waited on as a pseudo object.
                    self.block(Wait { objects: vec![cs | 1], all: false, deadline: None });
                }
            }
            "RtlLeaveCriticalSection" => {
                let cs = self.arg(0);
                let rec = self.r32(cs + 0x14).saturating_sub(1);
                self.w32(cs + 0x14, rec);
                let lock = self.r32(cs + 0x10);
                self.w32(cs + 0x10, lock.wrapping_sub(1));
                if rec == 0 {
                    self.w32(cs + 0x18, 0);
                }
                self.ret(STATUS_SUCCESS);
            }

            // ---------------------------------------------------------- spin locks / IRQL (one core view)
            "KeRaiseIrqlToDpcLevel" | "KfAcquireSpinLock" | "KeAcquireSpinLockAtRaisedIrql" | "KeTryToAcquireSpinLockAtRaisedIrql" => self.ret(if name.starts_with("KeTry") { 1 } else { 0 }),
            "KfLowerIrql" | "KfReleaseSpinLock" | "KeReleaseSpinLockFromRaisedIrql" | "KeEnterCriticalRegion" | "KeLeaveCriticalRegion" => self.ret(0),

            // ---------------------------------------------------------- TLS
            "KeTlsAlloc" => match (0..64).find(|i| self.tls_used & (1u64 << i) == 0) {
                Some(i) => {
                    self.tls_used |= 1 << i;
                    self.ret(i);
                }
                None => self.ret(u32::MAX),
            },
            "KeTlsFree" => {
                let i = self.arg(0);
                if i < 64 {
                    self.tls_used &= !(1 << i);
                }
                self.ret(1);
            }
            "KeTlsGetValue" => {
                let i = self.arg(0);
                let base = self.threads[self.current].tls_dynamic;
                let v = if i < 64 { self.r32(base + i * 4) } else { 0 };
                self.ret(v);
            }
            "KeTlsSetValue" => {
                let (i, v) = (self.arg(0), self.arg(1));
                let base = self.threads[self.current].tls_dynamic;
                if i < 64 {
                    self.w32(base + i * 4, v);
                }
                self.ret((i < 64) as u32);
            }

            // ---------------------------------------------------------- time
            "KeQueryPerformanceFrequency" => self.ret(50_000_000),
            "KeQuerySystemTime" => {
                let (p, t) = (self.arg(0), self.system_time());
                self.w64(p, t);
                self.ret(STATUS_SUCCESS);
            }
            "KeQueryInterruptTime" => {
                let t = self.now();
                self.ret64(t);
            }
            "RtlTimeToTimeFields" => {
                let (tp, fp) = (self.arg(0), self.arg(1));
                let f = time_fields(self.r64(tp));
                for (i, v) in f.iter().enumerate() {
                    self.w16(fp + i as u32 * 2, *v);
                }
                self.ret(STATUS_SUCCESS);
            }
            "RtlTimeFieldsToTime" => {
                let (fp, tp) = (self.arg(0), self.arg(1));
                let f: Vec<u16> = (0..7).map(|i| self.r16(fp + i * 2)).collect();
                let t = fields_to_time(&f);
                self.w64(tp, t);
                self.ret(1);
            }
            "KeDelayExecutionThread" => {
                let interval = self.arg(2);
                let existing = self.threads[self.current].wait.clone();
                match existing {
                    Some(w) if w.deadline.is_some_and(|d| self.now() >= d) => {
                        self.threads[self.current].wait = None;
                        self.ret(STATUS_SUCCESS);
                    }
                    Some(w) => self.block(w),
                    None => {
                        let d = self.deadline(interval).unwrap_or(self.now());
                        self.block(Wait { objects: vec![], all: false, deadline: Some(d.max(self.now() + 1)) });
                    }
                }
            }
            "NtYieldExecution" => {
                // First call yields; the retry (wait already recorded) returns.
                if self.threads[self.current].wait.take().is_some() {
                    self.ret(STATUS_SUCCESS);
                } else {
                    self.block(Wait { objects: vec![], all: false, deadline: Some(self.now()) });
                }
            }

            // ---------------------------------------------------------- threads
            "ExCreateThread" => {
                let (handle_ptr, stack, id_ptr, xapi, start, ctx, flags) = (self.arg(0), self.arg(1), self.arg(2), self.arg(3), self.arg(4), self.arg(5), self.arg(6));
                let suspended = flags & 1 != 0;
                let i = if xapi != 0 { self.create_thread(xapi, start, ctx, stack, suspended) } else { self.create_thread(start, ctx, 0, stack, suspended) };
                let (h, id) = (self.threads[i].handle, self.threads[i].id);
                if handle_ptr != 0 {
                    self.w32(handle_ptr, h);
                }
                if id_ptr != 0 {
                    self.w32(id_ptr, id);
                }
                self.ret(STATUS_SUCCESS);
            }
            "NtResumeThread" | "KeResumeThread" | "NtSuspendThread" | "KeSuspendThread" => {
                let obj = if name.starts_with("Nt") { self.object(self.arg(0)) } else { Some(self.arg(0)) };
                let prev_ptr = if name.starts_with("Nt") { self.arg(1) } else { 0 };
                let Some(obj) = obj else {
                    self.ret(STATUS_INVALID_HANDLE);
                    return Ok(true);
                };
                let resume = name.contains("Resume");
                let mut prev = 0;
                if let Some(t) = self.threads.iter_mut().find(|t| t.kthread == obj) {
                    prev = t.suspend;
                    if resume {
                        t.suspend = t.suspend.saturating_sub(1);
                        if t.suspend == 0 && t.state == ThreadState::Suspended {
                            t.state = ThreadState::Ready;
                        }
                    } else {
                        t.suspend += 1;
                        if t.state == ThreadState::Ready {
                            t.state = ThreadState::Suspended;
                        }
                    }
                }
                if prev_ptr != 0 {
                    self.w32(prev_ptr, prev);
                }
                self.ret(if name.starts_with("Nt") { STATUS_SUCCESS } else { prev });
            }
            "ExTerminateThread" => {
                let code = self.arg(0);
                self.exit_thread(code);
            }
            "ExTerminateTitleProcess" | "XamLoaderTerminateTitle" | "HalReturnToFirmware" => self.exit_code = Some(self.arg(0)),
            "KeBugCheck" | "KeBugCheckEx" => return Err(format!("The game crashed the kernel (bug check {:#x}).", self.arg(0))),
            "KeGetCurrentProcessType" => self.ret(2),
            "KeSetAffinityThread" | "KeQueryBasePriorityThread" | "KeSetBasePriorityThread" | "KeSetDisableBoostThread" | "KeSetPriorityThread" => self.ret(0),
            "RtlGetLastError" => {
                let k = self.kthread();
                let v = self.r32(k + 0x160);
                self.ret(v);
            }
            "RtlSetLastError" => {
                let (k, v) = (self.kthread(), self.arg(0));
                self.w32(k + 0x160, v);
            }

            // ---------------------------------------------------------- objects and handles
            "NtClose" => {
                let h = self.arg(0);
                let ok = self.handles.remove(&h).is_some();
                self.ret(if ok { STATUS_SUCCESS } else { STATUS_INVALID_HANDLE });
            }
            "NtDuplicateObject" => {
                let (h, out) = (self.arg(0), self.arg(1));
                match self.object(h) {
                    Some(o) => {
                        let n = self.new_handle(o);
                        self.w32(out, n);
                        self.ret(STATUS_SUCCESS);
                    }
                    None => self.ret(STATUS_INVALID_HANDLE),
                }
            }
            "ObReferenceObjectByHandle" => {
                let (h, out) = (self.arg(0), self.arg(2));
                match self.object(h) {
                    Some(o) => {
                        if out != 0 {
                            self.w32(out, o);
                        }
                        self.ret(STATUS_SUCCESS);
                    }
                    None => self.ret(STATUS_INVALID_HANDLE),
                }
            }
            "ObDereferenceObject" => self.ret(0),

            // ---------------------------------------------------------- events, semaphores, mutants
            "NtCreateEvent" => {
                let (hp, ty, initial) = (self.arg(0), self.arg(2), self.arg(3));
                let o = self.alloc_pool(0x10);
                self.w8(o, if ty == 0 { EVENT_NOTIFICATION } else { EVENT_SYNC });
                self.w32(o + 4, (initial != 0) as u32);
                let h = self.new_handle(o);
                self.w32(hp, h);
                self.ret(STATUS_SUCCESS);
            }
            "KeInitializeEvent" => {
                let (o, ty, state) = (self.arg(0), self.arg(1), self.arg(2));
                self.w8(o, if ty == 0 { EVENT_NOTIFICATION } else { EVENT_SYNC });
                self.w32(o + 4, (state != 0) as u32);
                self.w32(o + 8, o + 8);
                self.w32(o + 12, o + 8);
            }
            "NtSetEvent" | "NtClearEvent" | "NtPulseEvent" => {
                let (h, prev_ptr) = (self.arg(0), self.arg(1));
                let Some(o) = self.object(h) else {
                    self.ret(STATUS_INVALID_HANDLE);
                    return Ok(true);
                };
                let prev = self.r32(o + 4);
                self.w32(o + 4, (name == "NtSetEvent") as u32);
                if name == "NtSetEvent" && prev_ptr != 0 {
                    self.w32(prev_ptr, prev);
                }
                self.ret(STATUS_SUCCESS);
            }
            "KeSetEvent" | "KeResetEvent" | "KePulseEvent" => {
                let o = self.arg(0);
                let prev = self.r32(o + 4);
                self.w32(o + 4, (name == "KeSetEvent") as u32);
                self.ret(prev);
            }
            "NtCreateSemaphore" => {
                let (hp, count, limit) = (self.arg(0), self.arg(2), self.arg(3));
                let o = self.alloc_pool(0x14);
                self.w8(o, SEMAPHORE);
                self.w32(o + 4, count);
                self.w32(o + 0x10, limit);
                let h = self.new_handle(o);
                self.w32(hp, h);
                self.ret(STATUS_SUCCESS);
            }
            "KeInitializeSemaphore" => {
                let (o, count, limit) = (self.arg(0), self.arg(1), self.arg(2));
                self.w8(o, SEMAPHORE);
                self.w32(o + 4, count);
                self.w32(o + 0x10, limit);
            }
            "NtReleaseSemaphore" | "KeReleaseSemaphore" => {
                let (o, n, prev_ptr) = if name == "NtReleaseSemaphore" { (self.object(self.arg(0)), self.arg(1), self.arg(2)) } else { (Some(self.arg(0)), self.arg(2), 0) };
                let Some(o) = o else {
                    self.ret(STATUS_INVALID_HANDLE);
                    return Ok(true);
                };
                let prev = self.r32(o + 4);
                let limit = self.r32(o + 0x10);
                self.w32(o + 4, prev.saturating_add(n).min(limit.max(1)));
                if prev_ptr != 0 {
                    self.w32(prev_ptr, prev);
                }
                self.ret(if name == "NtReleaseSemaphore" { STATUS_SUCCESS } else { prev });
            }
            "NtCreateMutant" => {
                let (hp, owned) = (self.arg(0), self.arg(2));
                let o = self.alloc_pool(0x20);
                self.w8(o, MUTANT);
                if owned != 0 {
                    self.w32(o + 4, 0);
                    let k = self.kthread();
                    self.mutant_owner.insert(o, k);
                } else {
                    self.w32(o + 4, 1);
                }
                let h = self.new_handle(o);
                self.w32(hp, h);
                self.ret(STATUS_SUCCESS);
            }
            "NtReleaseMutant" => {
                let Some(o) = self.object(self.arg(0)) else {
                    self.ret(STATUS_INVALID_HANDLE);
                    return Ok(true);
                };
                let s = self.r32(o + 4) as i32 + 1;
                self.w32(o + 4, s as u32);
                if s > 0 {
                    self.mutant_owner.remove(&o);
                }
                self.ret(STATUS_SUCCESS);
            }

            // ---------------------------------------------------------- waits
            "NtWaitForSingleObjectEx" | "KeWaitForSingleObject" => {
                let (obj, timeout) = if name.starts_with("Nt") { (self.object(self.arg(0)), self.arg(3)) } else { (Some(self.arg(0)), self.arg(4)) };
                match obj {
                    Some(o) => self.wait(vec![o], false, timeout),
                    None => self.ret(STATUS_INVALID_HANDLE),
                }
            }
            "NtWaitForMultipleObjectsEx" | "KeWaitForMultipleObjects" => {
                let (count, list, ty) = (self.arg(0), self.arg(1), self.arg(2));
                let timeout = if name.starts_with("Nt") { self.arg(5) } else { self.arg(6) };
                let mut objs = Vec::new();
                for i in 0..count.min(64) {
                    let v = self.r32(list + i * 4);
                    let o = if name.starts_with("Nt") { self.object(v) } else { Some(v) };
                    match o {
                        Some(o) => objs.push(o),
                        None => {
                            self.ret(STATUS_INVALID_HANDLE);
                            return Ok(true);
                        }
                    }
                }
                self.wait(objs, ty == 0, timeout);
            }
            "NtSignalAndWaitForSingleObjectEx" => {
                let (sig, w, timeout) = (self.arg(0), self.arg(1), self.arg(4));
                if self.threads[self.current].wait.is_none() {
                    if let Some(o) = self.object(sig) {
                        self.signal(o);
                    }
                }
                match self.object(w) {
                    Some(o) => self.wait(vec![o], false, timeout),
                    None => self.ret(STATUS_INVALID_HANDLE),
                }
            }

            // ---------------------------------------------------------- strings and memory helpers
            "RtlInitAnsiString" => {
                let (dst, src) = (self.arg(0), self.arg(1));
                let len = if src == 0 { 0 } else { self.read_cstr(src).len() as u16 };
                self.w16(dst, len);
                self.w16(dst + 2, if src == 0 { 0 } else { len + 1 });
                self.w32(dst + 4, src);
            }
            "RtlInitUnicodeString" => {
                let (dst, src) = (self.arg(0), self.arg(1));
                let mut n = 0u32;
                while src != 0 && self.r16(src + n * 2) != 0 && n < 0x7FFF {
                    n += 1;
                }
                self.w16(dst, (n * 2) as u16);
                self.w16(dst + 2, if src == 0 { 0 } else { (n * 2 + 2) as u16 });
                self.w32(dst + 4, src);
            }
            "RtlFreeAnsiString" | "RtlFreeUnicodeString" => self.ret(0),
            "RtlNtStatusToDosError" => {
                let s = self.arg(0);
                let e = match s {
                    0 => 0,
                    STATUS_TIMEOUT => 258,
                    STATUS_INVALID_HANDLE => 6,
                    STATUS_INVALID_PARAMETER => 87,
                    STATUS_NO_MEMORY => 8,
                    STATUS_OBJECT_NAME_NOT_FOUND => 2,
                    _ if s & 0xFFFF_0000 == 0x8007_0000 => s & 0xFFFF,
                    _ => 317,
                };
                self.ret(e);
            }
            "RtlFillMemoryUlong" => {
                let (dst, len, pat) = (self.arg(0), self.arg(1), self.arg(2));
                for i in 0..len / 4 {
                    self.w32(dst + i * 4, pat);
                }
            }
            "RtlCompareMemoryUlong" => {
                let (src, len, pat) = (self.arg(0), self.arg(1), self.arg(2));
                let mut n = 0;
                while n + 4 <= len && self.r32(src + n) == pat {
                    n += 4;
                }
                self.ret(n);
            }
            "RtlUpcaseUnicodeChar" => {
                let c = self.arg(0) as u16;
                let u = char::from_u32(c as u32).map(|ch| ch.to_uppercase().next().unwrap_or(ch) as u32).unwrap_or(c as u32);
                self.ret(u & 0xFFFF);
            }
            "RtlMultiByteToUnicodeN" => {
                let (dst, dst_size, res, src, len) = (self.arg(0), self.arg(1), self.arg(2), self.arg(3), self.arg(4));
                let n = len.min(dst_size / 2);
                for i in 0..n {
                    let c = self.r8(src + i) as u16;
                    self.w16(dst + i * 2, c);
                }
                if res != 0 {
                    self.w32(res, n * 2);
                }
                self.ret(STATUS_SUCCESS);
            }
            "RtlUnicodeToMultiByteN" => {
                let (dst, dst_size, res, src, len) = (self.arg(0), self.arg(1), self.arg(2), self.arg(3), self.arg(4));
                let n = (len / 2).min(dst_size);
                for i in 0..n {
                    let c = self.r16(src + i * 2);
                    self.w8(dst + i, if c < 0x100 { c as u8 } else { b'?' });
                }
                if res != 0 {
                    self.w32(res, n);
                }
                self.ret(STATUS_SUCCESS);
            }
            "RtlComputeCrc32" => {
                let (mut crc, data, len) = (!self.arg(0), self.arg(1), self.arg(2));
                for i in 0..len {
                    crc ^= self.r8(data + i) as u32;
                    for _ in 0..8 {
                        crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
                    }
                }
                self.ret(!crc);
            }
            "DbgPrint" => {
                let s = self.format(self.arg(0), 1);
                self.log.push(format!("DbgPrint: {}", s.trim_end()));
                self.ret(STATUS_SUCCESS);
            }
            "sprintf" | "_snprintf" => {
                let (dst, fmt_i, cap) = if name == "sprintf" { (self.arg(0), 1, usize::MAX) } else { (self.arg(0), 2, self.arg(1) as usize) };
                let s = self.format(self.arg(fmt_i), fmt_i + 1);
                let bytes = s.as_bytes();
                let n = bytes.len().min(cap.saturating_sub(1));
                self.write_bytes(dst, &bytes[..n]);
                if cap > 0 {
                    self.w8(dst + n as u32, 0);
                }
                self.ret(n as u32);
            }

            // ---------------------------------------------------------- XEX / modules / config
            "RtlImageXexHeaderField" => {
                let (hdr, key) = (self.arg(0), self.arg(1));
                let count = self.r32(hdr + 0x14);
                let mut v = 0;
                for i in 0..count.min(0x400) {
                    if self.r32(hdr + 0x18 + i * 8) == key {
                        let val = self.r32(hdr + 0x1C + i * 8);
                        v = if key & 0xFF <= 1 { val } else { hdr + val };
                        break;
                    }
                }
                self.ret(v);
            }
            "XexCheckExecutablePrivilege" => {
                let bit = self.arg(0);
                let flags = self.xex.as_ref().and_then(|x| x.headers.get(&crate::xex::key::SYSTEM_FLAGS).copied()).unwrap_or(0);
                self.ret((bit < 32 && flags & (1 << bit) != 0) as u32);
            }
            "XexGetModuleHandle" => {
                let (np, out) = (self.arg(0), self.arg(1));
                let n = if np == 0 { String::new() } else { self.read_cstr(np).to_ascii_lowercase() };
                let h = match n.as_str() {
                    "" => self.ldr_entry,
                    "xboxkrnl.exe" => 0x8001_0000,
                    "xam.xex" => 0x8002_0000,
                    _ => 0,
                };
                self.w32(out, h);
                self.ret(if h == 0 { STATUS_NOT_FOUND } else { STATUS_SUCCESS });
            }
            "XexGetProcedureAddress" => {
                let (h, ordinal, out) = (self.arg(0), self.arg(1), self.arg(2));
                let module = match h {
                    0x8001_0000 => "xboxkrnl.exe",
                    0x8002_0000 => "xam.xex",
                    _ => {
                        self.ret(STATUS_NOT_FOUND);
                        return Ok(true);
                    }
                };
                match crate::exports::lookup(module, ordinal as u16) {
                    Some((n, false)) => {
                        let a = self.add_thunk(module, ordinal as u16, n);
                        self.w32(out, a);
                        self.ret(STATUS_SUCCESS);
                    }
                    Some((n, true)) => {
                        let a = self.variable(module, ordinal as u16, n);
                        self.w32(out, a);
                        self.ret(STATUS_SUCCESS);
                    }
                    None => self.ret(STATUS_NOT_FOUND),
                }
            }
            "ExGetXConfigSetting" => {
                let (buf, size, req) = (self.arg(2), self.arg(3), self.arg(4));
                if buf != 0 {
                    self.fill(buf, size, 0);
                }
                if req != 0 {
                    self.w16(req, size.min(4) as u16);
                }
                self.ret(STATUS_SUCCESS);
            }
            "XGetAVPack" => self.ret(6),
            "XGetGameRegion" => self.ret(0xFFFF),
            "XGetLanguage" => self.ret(1),
            "XamGetSystemVersion" => self.ret(0x2000_4000),
            "XGetVideoMode" | "VdQueryVideoMode" => {
                let p = self.arg(0);
                self.video_mode(p);
            }
            "VdGetCurrentDisplayInformation" => {
                let p = self.arg(0);
                self.fill(p, 0x58, 0);
                self.w16(p, 1280);
                self.w16(p + 2, 720);
                self.w16(p + 0x48, 1280);
                self.w16(p + 0x4A, 720);
                self.w32(p + 0x50, 60);
            }
            "XamGetExecutionId" => {
                let out = self.arg(0);
                let info = self.alloc_pool(0x18);
                let (media, version, base, title, platform, ty, disc, count) = match self.xex.as_ref().and_then(|x| x.execution.clone()) {
                    Some(e) => (e.media_id, e.version, e.base_version, e.title_id, e.platform, e.executable_type, e.disc_number, e.disc_count),
                    None => (0, 0, 0, 0, 0, 0, 1, 1),
                };
                self.w32(info, media);
                self.w32(info + 4, version);
                self.w32(info + 8, base);
                self.w32(info + 12, title);
                self.write_bytes(info + 16, &[platform, ty, disc, count]);
                self.w32(out, info);
                self.ret(STATUS_SUCCESS);
            }
            "XamLoaderGetLaunchData" => self.ret(0x0000_048F),

            // ---------------------------------------------------------- users and input (no profile signed in)
            "XamUserGetSigninState" => self.ret(0),
            "XamUserGetXUID" | "XamUserGetName" => self.ret(ERROR_NO_SUCH_USER),
            "XamInputGetCapabilities" => {
                let (user, caps) = (self.arg(0), self.arg(2));
                if user & 0xFF == 0 {
                    self.fill(caps, 0x14, 0);
                    self.w8(caps, 1); // gamepad
                    self.w16(caps + 4, 0xF3FF);
                    self.ret(0);
                } else {
                    self.ret(ERROR_DEVICE_NOT_CONNECTED);
                }
            }
            "XamInputGetState" => {
                let (user, state) = (self.arg(0), self.arg(2));
                if user & 0xFF == 0 {
                    let packet = (self.frames & 0xFFFF_FFFF) as u32;
                    self.fill(state, 0x10, 0);
                    self.w32(state, packet);
                    self.w16(state + 4, self.pad_buttons);
                    self.ret(0);
                } else {
                    self.ret(ERROR_DEVICE_NOT_CONNECTED);
                }
            }
            "XamInputSetState" | "XamEnableInactivityProcessing" | "XamResetInactivity" => self.ret(0),
            "XamNotifyCreateListener" => {
                let o = self.alloc_pool(0x10);
                self.w8(o, EVENT_NOTIFICATION);
                let h = self.new_handle(o);
                self.ret(h);
            }
            "XNotifyGetNext" => self.ret(0),

            // ---------------------------------------------------------- audio (no output yet)
            "XAudioGetSpeakerConfig" => {
                let p = self.arg(0);
                self.w32(p, 0x0001_0001);
                self.ret(0);
            }
            "XAudioRegisterRenderDriverClient" => {
                let out = self.arg(1);
                self.w32(out, 0x4155_0000);
                self.ret(0);
            }
            "XAudioUnregisterRenderDriverClient" | "XAudioSubmitRenderDriverFrame" => self.ret(0),

            // ---------------------------------------------------------- video (GPU still to come)
            "VdInitializeEngines" | "VdIsHSIOTrainingSucceeded" => self.ret(1),
            "VdSwap" => {
                self.frames += 1;
                self.ret(0);
            }
            "VdRetrainEDRAM" | "VdRetrainEDRAMWorker" | "VdPersistDisplay" | "VdShutdownEngines" | "VdSetDisplayMode" | "VdEnableDisableClockGating" => self.ret(0),
            "VdQueryVideoFlags" => self.ret(3),

            _ => return Ok(false),
        }
        Ok(true)
    }

    fn video_mode(&mut self, p: u32) {
        self.fill(p, 0x30, 0);
        self.w32(p, 1280);
        self.w32(p + 4, 720);
        self.w32(p + 8, 0);
        self.w32(p + 0xC, 1);
        self.w32(p + 0x10, 1);
        self.w32(p + 0x14, 60f32.to_bits());
        self.w32(p + 0x18, 1);
    }

    /// Absolute deadline (100 ns ticks) for an NT timeout pointer; None = wait forever.
    fn deadline(&self, timeout_ptr: u32) -> Option<u64> {
        if timeout_ptr == 0 {
            return None;
        }
        let t = self.r64(timeout_ptr) as i64;
        Some(if t < 0 { self.now() + t.unsigned_abs() } else { (t as u64).saturating_sub(self.system_time()) + self.now() })
    }

    fn signaled(&self, o: u32, me: u32) -> bool {
        if o & 1 == 1 {
            // Critical-section pseudo object: free or already ours.
            let owner = self.r32((o & !1) + 0x18);
            return owner == 0 || owner == me;
        }
        let state = self.r32(o + 4) as i32;
        match self.r8(o) {
            MUTANT => state > 0 || self.mutant_owner.get(&o) == Some(&me),
            _ => state > 0,
        }
    }

    fn acquire(&mut self, o: u32, me: u32) {
        match self.r8(o) {
            EVENT_SYNC => self.w32(o + 4, 0),
            SEMAPHORE => {
                let s = self.r32(o + 4);
                self.w32(o + 4, s.saturating_sub(1));
            }
            MUTANT => {
                let s = self.r32(o + 4) as i32;
                self.w32(o + 4, (s - 1) as u32);
                self.mutant_owner.insert(o, me);
            }
            _ => {}
        }
    }

    fn signal(&mut self, o: u32) {
        match self.r8(o) {
            SEMAPHORE => {
                let s = self.r32(o + 4);
                self.w32(o + 4, s + 1);
            }
            MUTANT => {
                let s = self.r32(o + 4) as i32 + 1;
                self.w32(o + 4, s as u32);
                if s > 0 {
                    self.mutant_owner.remove(&o);
                }
            }
            _ => self.w32(o + 4, 1),
        }
    }

    pub(crate) fn wait_satisfiable(&self, w: &Wait, me: u32) -> bool {
        if w.objects.is_empty() {
            return false;
        }
        if w.all {
            w.objects.iter().all(|&o| self.signaled(o, me))
        } else {
            w.objects.iter().any(|&o| self.signaled(o, me))
        }
    }

    /// Waits on objects: completes now if possible, otherwise blocks (and is retried).
    fn wait(&mut self, objects: Vec<u32>, all: bool, timeout_ptr: u32) {
        let me = self.kthread();
        let prior = self.threads[self.current].wait.take();
        let deadline = match &prior {
            Some(w) => w.deadline,
            None => self.deadline(timeout_ptr),
        };
        let ready: Vec<usize> = objects.iter().enumerate().filter(|(_, &o)| self.signaled(o, me)).map(|(i, _)| i).collect();
        if (all && ready.len() == objects.len()) || (!all && !ready.is_empty()) {
            if all {
                for &o in &objects {
                    self.acquire(o, me);
                }
                self.ret(0);
            } else {
                self.acquire(objects[ready[0]], me);
                self.ret(ready[0] as u32);
            }
            return;
        }
        if deadline.is_some_and(|d| self.now() >= d) {
            self.ret(STATUS_TIMEOUT);
            return;
        }
        let _ = THREAD;
        self.block(Wait { objects, all, deadline });
    }

    /// printf-style formatting with arguments taken from r3+`first_arg`.. (then the stack).
    fn format(&self, fmt_ptr: u32, first_arg: usize) -> String {
        let fmt = self.read_cstr(fmt_ptr);
        let mut out = String::new();
        let mut argi = first_arg;
        let mut next = |m: &Machine| {
            let v = if argi < 8 { m.arg64(argi) } else { m.r64(m.cpu.regs().r[1] as u32 + 0x50 + (argi as u32 - 8) * 8) };
            argi += 1;
            v
        };
        let mut chars = fmt.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            let mut spec = String::new();
            while let Some(&n) = chars.peek() {
                spec.push(n);
                chars.next();
                if n.is_ascii_alphabetic() && !matches!(n, 'l' | 'h' | 'I') || n == '%' {
                    break;
                }
            }
            let conv = spec.chars().last().unwrap_or('%');
            let width: usize = spec.trim_start_matches(['-', '0', '+', ' ', '#']).chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0);
            let zero = spec.starts_with('0');
            let s = match conv {
                '%' => "%".to_string(),
                'd' | 'i' => (next(self) as i32).to_string(),
                'u' => (next(self) as u32).to_string(),
                'x' => format!("{:x}", next(self) as u32),
                'X' => format!("{:X}", next(self) as u32),
                'p' => format!("{:08X}", next(self) as u32),
                'c' => ((next(self) as u8) as char).to_string(),
                's' => {
                    let p = next(self) as u32;
                    if p == 0 {
                        "(null)".into()
                    } else {
                        self.read_cstr(p)
                    }
                }
                'f' | 'g' | 'e' => "?".to_string(),
                _ => spec.clone(),
            };
            let pad = width.saturating_sub(s.len());
            out.push_str(&(if zero { "0" } else { " " }).repeat(pad));
            out.push_str(&s);
        }
        out
    }
}

/// Days between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const DAYS_1601_TO_1970: i64 = 134_774;

/// FILETIME -> [year, month, day, hour, minute, second, milliseconds, weekday].
fn time_fields(t: u64) -> [u16; 8] {
    let ms_total = t / 10_000;
    let ms = (ms_total % 1000) as u16;
    let secs = ms_total / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let weekday = ((days + 1) % 7) as u16; // 1601-01-01 was a Monday; Sunday = 0
    let z = days - DAYS_1601_TO_1970 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as i64;
    [year as u16, month as u16, day as u16, (rem / 3600) as u16, (rem / 60 % 60) as u16, (rem % 60) as u16, ms, weekday]
}

/// [year, month, day, hour, minute, second, ms] -> FILETIME.
fn fields_to_time(f: &[u16]) -> u64 {
    let (m, d) = (f[1] as i64, f[2] as i64);
    let y = f[0] as i64 - (m <= 2) as i64;
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468 + DAYS_1601_TO_1970;
    let secs = days as u64 * 86_400 + f[3] as u64 * 3600 + f[4] as u64 * 60 + f[5] as u64;
    (secs * 1000 + f[6] as u64) * 10_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_fields_round_trip() {
        // 2010-01-01 00:00:00 in FILETIME units.
        let t = 129_067_776_000_000_000u64;
        let f = time_fields(t);
        assert_eq!(&f[..7], &[2010, 1, 1, 0, 0, 0, 0]);
        assert_eq!(f[7], 5, "2010-01-01 was a Friday");
        assert_eq!(fields_to_time(&f[..7]), t);
    }
}
