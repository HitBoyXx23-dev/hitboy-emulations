//! Horizon's inter-process messages (HIPC / CMIF): how a game talks to every system service.
//!
//! A game connects to a named port (`sm:`), asks the service manager for a service by name, then
//! sends it numbered commands. Each message sits in the thread's 0x100-byte message buffer; the
//! kernel call `SendSyncRequest` hands it to the service and the reply is written back in place.
//! Services we haven't built yet stop the run with a message naming the service and command, so
//! the list of what a game needs is always one run away.

use crate::horizon::{Process, Stop};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Service {
    /// The service manager (`sm:`): hands out sessions to other services by name.
    Sm,
    /// `set`: user settings (language, region).
    Set,
    /// `set:sys`: system settings (firmware version, color set, ...).
    SetSys,
    /// `lm`: the logging manager.
    LmRoot,
    /// One logger session opened from `lm`.
    Logger,
    /// `time:u` / `time:s` / `time:a`: system time services.
    Time,
    /// `ISystemClock` sub-session from `time:*`.
    SystemClock,
    /// `ISteadyClock` sub-session from `time:*`.
    SteadyClock,
    /// `ITimeZoneService` sub-session from `time:*`.
    TimeZone,
    /// `apm`: performance manager.
    Apm,
    /// `ISession` opened from `apm`.
    ApmSession,
    /// `appletOE` / `appletAE`: application lifecycle service.
    Applet,
    /// `IApplicationProxy` opened from `appletOE`/`appletAE`.
    AppletProxy,
    /// `ICommonStateGetter` from `IApplicationProxy`.
    AppletCommonState,
    /// `IApplicationFunctions` from `IApplicationProxy`.
    AppletFunctions,
    /// `ISelfController` / `IWindowController` / `IAudioController` / `IDisplayController` / `ILibraryAppletCreator`.
    AppletSub(&'static str),
    /// `hid`: controller & input service.
    Hid,
    /// `fsp-srv`: file system proxy service.
    FspSrv,
    /// `IStorage` backed by the process's decrypted RomFS image.
    RomFsStorage,
    /// `vi:u` / `vi:m` / `vi:s`: visual interface root service.
    Vi,
    /// `IApplicationDisplayService` opened from `vi:*`.
    ViDisplay,
    /// Sub-services opened from `IApplicationDisplayService`.
    ViSub(&'static str),
    /// `nvdrv` / `nvdrv:a`: NVIDIA GPU driver service.
    NvDrv,
    /// `audout:u`: audio output root service.
    AudOutRoot,
    /// `IAudioOut` stream session.
    AudOut,
    /// `acc:u0` / `acc:u1`: user account service.
    Account,
    /// `IManagerForApplication` opened from `acc:*`.
    AccountManager,
    /// A service we recognise by name but haven't built.
    Unknown(String),
}

impl Service {
    pub fn by_name(name: &str) -> Service {
        match name {
            "set" => Service::Set,
            "set:sys" => Service::SetSys,
            "lm" => Service::LmRoot,
            "time:u" | "time:s" | "time:a" => Service::Time,
            "apm" => Service::Apm,
            "appletOE" | "appletAE" => Service::Applet,
            "hid" => Service::Hid,
            "fsp-srv" => Service::FspSrv,
            "vi:u" | "vi:m" | "vi:s" => Service::Vi,
            "nvdrv" | "nvdrv:a" | "nvdrv:s" | "nvdrv:t" => Service::NvDrv,
            "audout:u" => Service::AudOutRoot,
            "acc:u0" | "acc:u1" => Service::Account,
            other => Service::Unknown(other.to_string()),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Service::Sm => "sm:".into(),
            Service::Set => "set".into(),
            Service::SetSys => "set:sys".into(),
            Service::LmRoot => "lm".into(),
            Service::Logger => "lm (logger)".into(),
            Service::Time => "time:u".into(),
            Service::SystemClock => "time (ISystemClock)".into(),
            Service::SteadyClock => "time (ISteadyClock)".into(),
            Service::TimeZone => "time (ITimeZoneService)".into(),
            Service::Apm => "apm".into(),
            Service::ApmSession => "apm (ISession)".into(),
            Service::Applet => "appletOE".into(),
            Service::AppletProxy => "appletOE (IApplicationProxy)".into(),
            Service::AppletCommonState => "appletOE (ICommonStateGetter)".into(),
            Service::AppletFunctions => "appletOE (IApplicationFunctions)".into(),
            Service::AppletSub(sub) => format!("appletOE ({sub})"),
            Service::Hid => "hid".into(),
            Service::FspSrv => "fsp-srv".into(),
            Service::RomFsStorage => "fsp-srv (IStorage/RomFS)".into(),
            Service::Vi => "vi:u".into(),
            Service::ViDisplay => "vi (IApplicationDisplayService)".into(),
            Service::ViSub(sub) => format!("vi ({sub})"),
            Service::NvDrv => "nvdrv".into(),
            Service::AudOutRoot => "audout:u".into(),
            Service::AudOut => "audout:u (IAudioOut)".into(),
            Service::Account => "acc:u0".into(),
            Service::AccountManager => "acc (IManagerForApplication)".into(),
            Service::Unknown(n) => n.clone(),
        }
    }
}

/// A memory buffer passed alongside a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Buffer {
    pub addr: u64,
    pub size: u64,
    /// 'X' pointer (in), 'A' send (in), 'B' receive (out), 'W' exchange.
    pub kind: char,
}

#[derive(Debug)]
pub struct Request {
    /// HIPC message type: 2 close, 4/6 request, 5/7 control.
    pub kind: u32,
    pub pid: bool,
    pub copy_handles: Vec<u32>,
    pub move_handles: Vec<u32>,
    pub buffers: Vec<Buffer>,
    pub cmd: u32,
    /// Command arguments (the raw data after the 16-byte CMIF header).
    pub args: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct Reply {
    pub result: u32,
    pub data: Vec<u8>,
    pub copy_handles: Vec<u32>,
    pub move_handles: Vec<u32>,
}

fn word(m: &[u8], o: usize) -> Result<u32, String> {
    m.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| "A service message ran past its buffer.".to_string())
}

/// Reads one message out of a thread's message buffer.
pub fn parse(m: &[u8]) -> Result<Request, String> {
    let (w0, w1) = (word(m, 0)?, word(m, 4)?);
    let kind = w0 & 0xFFFF;
    let (nx, na, nb, nw) = ((w0 >> 16) & 0xF, (w0 >> 20) & 0xF, (w0 >> 24) & 0xF, (w0 >> 28) & 0xF);
    let raw_words = (w1 & 0x3FF) as usize;
    let mut off = 8;
    let (mut pid, mut copy_handles, mut move_handles) = (false, Vec::new(), Vec::new());
    if w1 >> 31 != 0 {
        let sp = word(m, 8)?;
        off = 12;
        if sp & 1 != 0 {
            pid = true;
            off += 8;
        }
        for _ in 0..(sp >> 1) & 0xF {
            copy_handles.push(word(m, off)?);
            off += 4;
        }
        for _ in 0..(sp >> 5) & 0xF {
            move_handles.push(word(m, off)?);
            off += 4;
        }
    }
    let mut buffers = Vec::new();
    for _ in 0..nx {
        let (a, b) = (word(m, off)?, word(m, off + 4)?);
        let addr = b as u64 | (((a >> 12) & 0xF) as u64) << 32 | (((a >> 6) & 7) as u64) << 36;
        buffers.push(Buffer { addr, size: (a >> 16) as u64, kind: 'X' });
        off += 8;
    }
    for (count, kind) in [(na, 'A'), (nb, 'B'), (nw, 'W')] {
        for _ in 0..count {
            let (size, lo, fl) = (word(m, off)?, word(m, off + 4)?, word(m, off + 8)?);
            let addr = lo as u64 | (((fl >> 28) & 0xF) as u64) << 32 | (((fl >> 2) & 7) as u64) << 36;
            buffers.push(Buffer { addr, size: size as u64 | (((fl >> 24) & 0xF) as u64) << 32, kind });
            off += 12;
        }
    }
    let raw_off = (off + 15) & !15;
    let raw = m.get(raw_off..raw_off + raw_words * 4).ok_or("A service message's data ran past its buffer.")?;
    let (mut cmd, mut args) = (0, Vec::new());
    if matches!(kind, 4..=7) {
        if raw.len() < 16 || &raw[..4] != b"SFCI" {
            return Err("A service message doesn't carry the expected header.".into());
        }
        cmd = u32::from_le_bytes(raw[8..12].try_into().unwrap());
        args = raw[16..].to_vec();
    }
    Ok(Request { kind, pid, copy_handles, move_handles, buffers, cmd, args })
}

/// Builds the reply message the game will read from its message buffer.
pub fn encode(r: &Reply) -> Vec<u8> {
    let mut raw = b"SFCO".to_vec();
    raw.extend_from_slice(&0u32.to_le_bytes());
    raw.extend_from_slice(&r.result.to_le_bytes());
    raw.extend_from_slice(&0u32.to_le_bytes());
    raw.extend_from_slice(&r.data);
    while raw.len() % 4 != 0 {
        raw.push(0);
    }
    let has_handles = !r.copy_handles.is_empty() || !r.move_handles.is_empty();
    let mut m = Vec::new();
    m.extend_from_slice(&0u32.to_le_bytes());
    m.extend_from_slice(&(((raw.len() / 4) as u32) | (has_handles as u32) << 31).to_le_bytes());
    if has_handles {
        m.extend_from_slice(&(((r.copy_handles.len() as u32) << 1) | ((r.move_handles.len() as u32) << 5)).to_le_bytes());
        for h in r.copy_handles.iter().chain(&r.move_handles) {
            m.extend_from_slice(&h.to_le_bytes());
        }
    }
    m.resize((m.len() + 15) & !15, 0);
    m.extend(raw);
    m
}

const SET_SYS_FIRMWARE: &[u8] = b"12.1.0";

impl Process {
    fn new_handle(&mut self, s: Service) -> u32 {
        let h = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(h, s);
        h
    }

    /// `ConnectToNamedPort`: only the service manager has a port.
    pub(crate) fn connect_named_port(&mut self, name_ptr: u64) -> Option<Stop> {
        let name: String = self.mem.read_bytes(name_ptr, 12).map(|b| b.iter().take_while(|&&c| c != 0).map(|&c| c as char).collect()).unwrap_or_default();
        if name == "sm:" {
            let h = self.new_handle(Service::Sm);
            self.cpu.x[0] = 0;
            self.cpu.x[1] = h as u64;
            None
        } else {
            Some(Stop::Unsupported(format!("The program asked for the \"{name}\" port, which isn't implemented yet.")))
        }
    }

    pub(crate) fn close_handle(&mut self, h: u32) {
        self.cpu.x[0] = if self.handles.remove(&h).is_some() || h == crate::horizon::MAIN_THREAD_HANDLE as u32 { 0 } else { 0xE401 };
    }

    /// `SendSyncRequest`: runs the message in the thread's buffer against the session's service.
    pub(crate) fn send_sync_request(&mut self, handle: u32) -> Option<Stop> {
        let tls = self.cpu.tpidrro_el0;
        let Some(svc) = self.handles.get(&handle).cloned() else {
            self.cpu.x[0] = 0xE401; // invalid handle
            return None;
        };
        let msg = self.mem.read_bytes(tls, 0x100)?;
        let req = match parse(&msg) {
            Ok(r) => r,
            Err(e) => return Some(Stop::Unsupported(e)),
        };
        let reply = match req.kind {
            2 => {
                self.handles.remove(&handle);
                self.cpu.x[0] = 0;
                return None;
            }
            5 | 7 => match req.cmd {
                // QueryPointerBufferSize
                3 => Reply { data: 0x8000u16.to_le_bytes().to_vec(), ..Reply::default() },
                // CloneCurrentObject / CloneCurrentObjectEx
                2 | 4 => Reply { move_handles: vec![self.new_handle(svc)], ..Reply::default() },
                c => return Some(Stop::Unsupported(format!("The program used session control command {c}, which isn't implemented yet."))),
            },
            4 | 6 => match self.call(&svc, &req) {
                Ok(r) => r,
                Err(e) => return Some(Stop::Unsupported(e)),
            },
            k => return Some(Stop::Unsupported(format!("The program sent a message of type {k}, which isn't implemented yet."))),
        };
        self.mem.write_bytes(tls, &encode(&reply));
        self.cpu.x[0] = 0;
        None
    }

    fn call(&mut self, svc: &Service, req: &Request) -> Result<Reply, String> {
        let unknown = || Err(format!("The program called command {} of the \"{}\" system service, which isn't implemented yet.", req.cmd, svc.name()));
        match (svc, req.cmd) {
            (Service::Sm, 0) => Ok(Reply::default()),
            (Service::Sm, 1) => {
                let raw = req.args.get(..8).ok_or("GetServiceHandle came without a service name.")?;
                let name: String = raw.iter().take_while(|&&c| c != 0).map(|&c| c as char).collect();
                let h = self.new_handle(Service::by_name(&name));
                Ok(Reply { move_handles: vec![h], ..Reply::default() })
            }
            (Service::Set, 0) => {
                // GetLanguageCode -> "en-US" (8-byte padded ASCII)
                Ok(Reply { data: b"en-US\0\0\0".to_vec(), ..Reply::default() })
            }
            (Service::Set, 4) => {
                // GetRegionCode -> 1 (USA)
                Ok(Reply { data: 1u32.to_le_bytes().to_vec(), ..Reply::default() })
            }
            (Service::SetSys, 3) | (Service::SetSys, 4) => {
                let out = req.buffers.iter().find(|b| b.kind == 'B' || b.kind == 'W').ok_or("GetFirmwareVersion came without an output buffer.")?;
                let mut v = vec![0u8; 0x100];
                v[0] = 12; // major
                v[1] = 1; // minor
                v[2] = 0; // micro
                v[0x08..0x0A].copy_from_slice(b"NX");
                let n = SET_SYS_FIRMWARE.len();
                v[0x68..0x68 + n].copy_from_slice(SET_SYS_FIRMWARE);
                let title = b"NintendoSDK Firmware for NX 12.1.0-1.0";
                v[0x80..0x80 + title.len()].copy_from_slice(title);
                if (v.len() as u64) > out.size || !self.mem.write_bytes(out.addr, &v) {
                    return Ok(Reply { result: 0x1A0A, ..Reply::default() }); // buffer too small / unmapped
                }
                Ok(Reply::default())
            }
            (Service::SetSys, 23) => {
                // GetColorSetId -> 1 (BasicBlack)
                Ok(Reply { data: 1u32.to_le_bytes().to_vec(), ..Reply::default() })
            }
            (Service::Time, 0) | (Service::Time, 1) | (Service::Time, 4) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::SystemClock)], ..Reply::default() })
            }
            (Service::Time, 2) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::SteadyClock)], ..Reply::default() })
            }
            (Service::Time, 3) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::TimeZone)], ..Reply::default() })
            }
            (Service::SystemClock, 0) => {
                // GetCurrentTime -> deterministic POSIX epoch timestamp (seconds)
                let ts: i64 = 1_700_000_000;
                Ok(Reply { data: ts.to_le_bytes().to_vec(), ..Reply::default() })
            }
            (Service::SteadyClock, 0) => {
                // GetCurrentTimePoint -> 8-byte seconds + 16-byte clock source UUID
                let mut data = Vec::with_capacity(24);
                data.extend_from_slice(&1_000i64.to_le_bytes());
                data.extend_from_slice(&[0x53, 0x57, 0x49, 0x54, 0x43, 0x48, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
                Ok(Reply { data, ..Reply::default() })
            }
            (Service::TimeZone, 0) => {
                // GetDeviceLocationName -> 36-byte location name ("UTC")
                let mut loc = vec![0u8; 36];
                loc[..3].copy_from_slice(b"UTC");
                Ok(Reply { data: loc, ..Reply::default() })
            }
            (Service::Apm, 0) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::ApmSession)], ..Reply::default() })
            }
            (Service::ApmSession, 0) => Ok(Reply::default()),
            (Service::Applet, 0) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::AppletProxy)], ..Reply::default() })
            }
            (Service::AppletProxy, 0) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::AppletCommonState)], ..Reply::default() })
            }
            (Service::AppletProxy, 1) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::AppletSub("ISelfController"))], ..Reply::default() })
            }
            (Service::AppletProxy, 2) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::AppletSub("IWindowController"))], ..Reply::default() })
            }
            (Service::AppletProxy, 3) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::AppletSub("IAudioController"))], ..Reply::default() })
            }
            (Service::AppletProxy, 4) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::AppletSub("IDisplayController"))], ..Reply::default() })
            }
            (Service::AppletProxy, 11) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::AppletSub("ILibraryAppletCreator"))], ..Reply::default() })
            }
            (Service::AppletProxy, 20) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::AppletFunctions)], ..Reply::default() })
            }
            (Service::AppletCommonState, 5) => {
                // GetOperationMode -> 0 (Handheld)
                Ok(Reply { data: vec![0u8], ..Reply::default() })
            }
            (Service::AppletCommonState, 6) => {
                // GetPerformanceMode -> 0 (Normal)
                Ok(Reply { data: 0u32.to_le_bytes().to_vec(), ..Reply::default() })
            }
            (Service::AppletCommonState, 9) => {
                // GetFocusState -> 1 (InFocus)
                Ok(Reply { data: vec![1u8], ..Reply::default() })
            }
            (Service::AppletFunctions, 40) => {
                // NotifyRunning -> 1 (true)
                Ok(Reply { data: vec![1u8], ..Reply::default() })
            }
            (Service::Hid, 100 | 102 | 103 | 108 | 120 | 122) => Ok(Reply::default()),
            (Service::FspSrv, 1) => Ok(Reply::default()), // SetCurrentProcess
            (Service::FspSrv, 18 | 51) => Ok(Reply { result: 0x202, ..Reply::default() }), // OpenSdCard / OpenSaveData -> PathNotFound
            (Service::FspSrv, 200) => {
                // OpenDataStorageByCurrentProcess -> IStorage backed by self.romfs
                if self.romfs.is_some() {
                    Ok(Reply { move_handles: vec![self.new_handle(Service::RomFsStorage)], ..Reply::default() })
                } else {
                    Ok(Reply { result: 0x202, ..Reply::default() })
                }
            }
            (Service::FspSrv, 1005) => Ok(Reply { data: 0u32.to_le_bytes().to_vec(), ..Reply::default() }), // GetGlobalAccessLogMode
            (Service::RomFsStorage, 0) => {
                // IStorage::Read(offset: u64, size: u64) -> writes into output buffer B/W
                let offset = req.args.get(0..8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0) as usize;
                let size = req.args.get(8..16).map(|b| u64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0) as usize;
                let out = req.buffers.iter().find(|b| b.kind == 'B' || b.kind == 'W').ok_or("IStorage::Read came without an output buffer.")?;
                let Some(romfs) = self.romfs.as_deref() else {
                    return Ok(Reply { result: 0x202, ..Reply::default() });
                };
                let to_read = size.min(out.size as usize).min(romfs.len().saturating_sub(offset));
                if to_read > 0 && !self.mem.write_bytes(out.addr, &romfs[offset..offset + to_read]) {
                    return Ok(Reply { result: 0x1A0A, ..Reply::default() });
                }
                Ok(Reply::default())
            }
            (Service::RomFsStorage, 4) => {
                // IStorage::GetSize() -> u64
                let sz = self.romfs.as_ref().map(|r| r.len() as u64).unwrap_or(0);
                Ok(Reply { data: sz.to_le_bytes().to_vec(), ..Reply::default() })
            }
            (Service::Vi, 0 | 1 | 2) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::ViDisplay)], ..Reply::default() })
            }
            (Service::ViDisplay, 100) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::ViSub("IHOSBinderDriver"))], ..Reply::default() })
            }
            (Service::ViDisplay, 101) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::ViSub("ISystemDisplayService"))], ..Reply::default() })
            }
            (Service::ViDisplay, 102) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::ViSub("IManagerDisplayService"))], ..Reply::default() })
            }
            (Service::ViDisplay, 1010) => {
                // OpenDisplay -> display_id = 0
                Ok(Reply { data: 0u64.to_le_bytes().to_vec(), ..Reply::default() })
            }
            (Service::ViDisplay, 1020 | 2021 | 2101) => Ok(Reply::default()),
            (Service::ViDisplay, 2020) => {
                // OpenLayer -> layer_id = 1
                Ok(Reply { data: 1u64.to_le_bytes().to_vec(), ..Reply::default() })
            }
            (Service::NvDrv, 0) => {
                // Open(path) -> (fd: u32, nv_error: u32)
                let mut data = Vec::with_capacity(8);
                data.extend_from_slice(&1u32.to_le_bytes());
                data.extend_from_slice(&0u32.to_le_bytes());
                Ok(Reply { data, ..Reply::default() })
            }
            (Service::NvDrv, 2 | 3 | 8) => {
                // Close / Initialize / SetAruid -> nv_error = 0
                Ok(Reply { data: 0u32.to_le_bytes().to_vec(), ..Reply::default() })
            }
            (Service::AudOutRoot, 0) => Ok(Reply { data: 0u32.to_le_bytes().to_vec(), ..Reply::default() }),
            (Service::AudOutRoot, 1) => {
                // OpenAudioOut -> (sample_rate=48000, channels=2, pcm_format=2, state=1) + IAudioOut handle
                let mut data = Vec::with_capacity(16);
                for v in [48_000u32, 2, 2, 1] {
                    data.extend_from_slice(&v.to_le_bytes());
                }
                Ok(Reply { data, move_handles: vec![self.new_handle(Service::AudOut)], ..Reply::default() })
            }
            (Service::AudOut, 0) => Ok(Reply { data: 0u32.to_le_bytes().to_vec(), ..Reply::default() }),
            (Service::AudOut, 1 | 2) => Ok(Reply::default()),
            (Service::AudOut, 3 | 7) => {
                // AppendAudioOutBuffer / AppendAudioOutBufferAuto:
                // args[0..8] = tag, input buffer A/X holds AudioOutBuffer descriptor (next:u64, data_addr:u64, buf_size:u64, data_size:u64, data_offset:u64)
                let tag = req.args.get(0..8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
                if let Some(desc_buf) = req.buffers.iter().find(|b| matches!(b.kind, 'A' | 'X')) {
                    if let Some(desc) = self.mem.read_bytes(desc_buf.addr, desc_buf.size.min(40) as usize) {
                        if desc.len() >= 32 {
                            let data_addr = u64::from_le_bytes(desc[8..16].try_into().unwrap());
                            let data_size = u64::from_le_bytes(desc[24..32].try_into().unwrap()).min(65536) as usize;
                            if data_addr != 0 && data_size >= 4 && self.audio_samples.len() < 48_000 * 2 {
                                if let Some(pcm) = self.mem.read_bytes(data_addr, data_size) {
                                    for c in pcm.chunks_exact(4) {
                                        let l = i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0;
                                        let r = i16::from_le_bytes([c[2], c[3]]) as f32 / 32768.0;
                                        self.audio_samples.push(l);
                                        self.audio_samples.push(r);
                                    }
                                }
                            }
                        }
                    }
                }
                self.released_audio_tags.push(tag);
                Ok(Reply::default())
            }
            (Service::AudOut, 4 | 8) => {
                // GetReleasedAudioOutBuffer / GetReleasedAudioOutBufferAuto
                let mut written = 0u32;
                if let Some(out) = req.buffers.iter().find(|b| b.kind == 'B' || b.kind == 'W') {
                    let cap = (out.size as usize) / 8;
                    let n = cap.min(self.released_audio_tags.len());
                    for (i, tag) in self.released_audio_tags.drain(..n).enumerate() {
                        let _ = self.mem.write_bytes(out.addr + (i as u64) * 8, &tag.to_le_bytes());
                        written += 1;
                    }
                }
                Ok(Reply { data: written.to_le_bytes().to_vec(), ..Reply::default() })
            }
            (Service::Account, 0) => Ok(Reply { data: 1u32.to_le_bytes().to_vec(), ..Reply::default() }),
            (Service::Account, 1) => Ok(Reply { data: vec![1u8], ..Reply::default() }),
            (Service::Account, 2) => {
                // ListAllUsers -> write one synthetic 16-byte UID into output buffer
                if let Some(out) = req.buffers.iter().find(|b| b.kind == 'B' || b.kind == 'W') {
                    let uid = [1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
                    if out.size >= 16 {
                        let _ = self.mem.write_bytes(out.addr, &uid);
                    }
                }
                Ok(Reply::default())
            }
            (Service::Account, 100) => Ok(Reply::default()),
            (Service::Account, 101) => {
                Ok(Reply { move_handles: vec![self.new_handle(Service::AccountManager)], ..Reply::default() })
            }
            (Service::LmRoot, 0) => Ok(Reply { move_handles: vec![self.new_handle(Service::Logger)], ..Reply::default() }),
            (Service::Logger, 0) => {
                let buf = req.buffers.iter().find(|b| matches!(b.kind, 'X' | 'A')).ok_or("Log came without a message buffer.")?;
                if let Some(bytes) = self.mem.read_bytes(buf.addr, buf.size.min(0x10000) as usize) {
                    // The packet is a binary header plus text fields; keep the readable runs.
                    let mut text = String::new();
                    for run in bytes.split(|&b| !(0x20..0x7F).contains(&b) && b != b'\n') {
                        if run.len() >= 2 {
                            text.push_str(&String::from_utf8_lossy(run));
                            text.push(' ');
                        }
                    }
                    if !text.is_empty() {
                        self.debug_output.push_str(text.trim_end());
                        self.debug_output.push('\n');
                    }
                }
                Ok(Reply::default())
            }
            _ => unknown(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::horizon::{Process, Stop, HEAP_BASE};
    use crate::nso::{Nso, Segment};

    fn process() -> Process {
        let nso = Nso {
            segments: [Segment { mem_off: 0, data: vec![0; 16] }, Segment { mem_off: 0x1000, data: vec![0; 0x100] }, Segment { mem_off: 0x2000, data: vec![] }],
            bss_size: 0x1000,
            build_id: [0; 0x20],
        };
        Process::load(&nso).unwrap()
    }

    /// Builds a request message: header, optional move handles, buffers, then SFCI data.
    fn request(kind: u32, cmd: u32, args: &[u8], pid: bool, buffers: &[(char, u64, u64)]) -> Vec<u8> {
        let (mut nx, mut na, mut nb) = (0u32, 0u32, 0u32);
        for &(k, _, _) in buffers {
            match k {
                'X' => {
                    nx += 1;
                }
                'A' => na += 1,
                _ => nb += 1,
            }
        }
        let mut m = Vec::new();
        m.extend_from_slice(&(kind | nx << 16 | na << 20 | nb << 24).to_le_bytes());
        let mut raw = b"SFCI".to_vec();
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw.extend_from_slice(&cmd.to_le_bytes());
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw.extend_from_slice(args);
        while raw.len() % 4 != 0 {
            raw.push(0);
        }
        m.extend_from_slice(&(((raw.len() / 4) as u32) | (pid as u32) << 31).to_le_bytes());
        if pid {
            m.extend_from_slice(&1u32.to_le_bytes()); // special header: has pid
            m.extend_from_slice(&0u64.to_le_bytes());
        }
        for &(k, addr, size) in buffers {
            match k {
                'X' => {
                    m.extend_from_slice(&(((size as u32) << 16) | (((addr >> 32) & 0xF) as u32) << 12 | (((addr >> 36) & 7) as u32) << 6).to_le_bytes());
                    m.extend_from_slice(&(addr as u32).to_le_bytes());
                }
                _ => {
                    m.extend_from_slice(&(size as u32).to_le_bytes());
                    m.extend_from_slice(&(addr as u32).to_le_bytes());
                    m.extend_from_slice(&0u32.to_le_bytes());
                }
            }
        }
        m.resize((m.len() + 15) & !15, 0);
        m.extend(raw);
        m
    }

    fn send(p: &mut Process, h: u32, msg: &[u8]) -> Option<Stop> {
        let tls = p.cpu.tpidrro_el0;
        p.mem.write_bytes(tls, msg);
        p.cpu.x[0] = h as u64;
        p.svc(0x21)
    }

    fn reply_of(p: &mut Process) -> Request2 {
        let tls = p.cpu.tpidrro_el0;
        let m = p.mem.read_bytes(tls, 0x100).unwrap();
        let w1 = u32::from_le_bytes(m[4..8].try_into().unwrap());
        let mut off = 8;
        let mut moves = Vec::new();
        if w1 >> 31 != 0 {
            let sp = u32::from_le_bytes(m[8..12].try_into().unwrap());
            off = 12 + ((sp >> 1) & 0xF) as usize * 4;
            for k in 0..(sp >> 5) & 0xF {
                moves.push(u32::from_le_bytes(m[off + 4 * k as usize..][..4].try_into().unwrap()));
            }
            off += moves.len() * 4;
        }
        let raw = &m[(off + 15) & !15..];
        assert_eq!(&raw[..4], b"SFCO");
        Request2 { result: u32::from_le_bytes(raw[8..12].try_into().unwrap()), data: raw[16..].to_vec(), moves }
    }

    struct Request2 {
        result: u32,
        data: Vec<u8>,
        moves: Vec<u32>,
    }

    fn connect_sm(p: &mut Process) -> u32 {
        p.mem.write_bytes(0x0100_0800, b"sm:\0");
        p.cpu.x[1] = 0x0100_0800;
        assert!(p.svc(0x1F).is_none());
        assert_eq!(p.cpu.x[0], 0);
        p.cpu.x[1] as u32
    }

    #[test]
    fn messages_round_trip() {
        let m = request(4, 7, &[1, 2, 3, 4, 5, 6, 7, 8], true, &[('X', 0x1_2000_0000, 0x40)]);
        let r = parse(&m).unwrap();
        assert_eq!((r.kind, r.cmd, r.pid), (4, 7, true));
        assert_eq!(r.args, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(r.buffers, vec![Buffer { addr: 0x1_2000_0000, size: 0x40, kind: 'X' }]);
        assert!(parse(&[0xFF; 4]).is_err());
    }

    #[test]
    fn the_service_manager_hands_out_set_sys_and_it_answers() {
        let mut p = process();
        let sm = connect_sm(&mut p);
        // session control: QueryPointerBufferSize
        assert!(send(&mut p, sm, &request(5, 3, &[], false, &[])).is_none());
        assert_eq!(reply_of(&mut p).data[..2], [0x00, 0x80]);
        // sm: Initialize (with pid), then GetServiceHandle("set:sys")
        assert!(send(&mut p, sm, &request(4, 0, &[0; 8], true, &[])).is_none());
        assert_eq!(reply_of(&mut p).result, 0);
        assert!(send(&mut p, sm, &request(4, 1, b"set:sys\0", false, &[])).is_none());
        let set_sys = reply_of(&mut p).moves[0];
        // set:sys GetFirmwareVersion into a buffer on the heap
        p.cpu.x[1] = 0x20_0000;
        p.svc(0x01);
        let out = HEAP_BASE;
        assert!(send(&mut p, set_sys, &request(4, 3, &[], false, &[('B', out, 0x100)])).is_none());
        assert_eq!(reply_of(&mut p).result, 0);
        let fw = p.mem.read_bytes(out, 0x100).unwrap();
        assert_eq!((fw[0], fw[1]), (12, 1));
        assert_eq!(&fw[8..10], b"NX");
    }

    #[test]
    fn logger_text_reaches_the_debug_output() {
        let mut p = process();
        let sm = connect_sm(&mut p);
        send(&mut p, sm, &request(4, 1, b"lm\0\0\0\0\0\0", false, &[]));
        let lm = reply_of(&mut p).moves[0];
        send(&mut p, lm, &request(4, 0, &[0; 8], true, &[]));
        let logger = reply_of(&mut p).moves[0];
        p.cpu.x[1] = 0x20_0000;
        p.svc(0x01);
        let mut packet = vec![1u8, 0, 0, 0, 0xFF, 0x03];
        packet.extend_from_slice(b"hello from the game");
        p.mem.write_bytes(HEAP_BASE, &packet);
        assert!(send(&mut p, logger, &request(4, 0, &[], false, &[('X', HEAP_BASE, packet.len() as u64)])).is_none());
        assert!(p.debug_output.contains("hello from the game"), "{:?}", p.debug_output);
    }

    #[test]
    fn unknown_services_and_handles_are_named() {
        let mut p = process();
        let sm = connect_sm(&mut p);
        send(&mut p, sm, &request(4, 1, b"bsd:u\0\0\0", false, &[]));
        let bsd = reply_of(&mut p).moves[0];
        match send(&mut p, bsd, &request(4, 1, &[], false, &[])) {
            Some(Stop::Unsupported(m)) => assert!(m.contains("bsd:u") && m.contains("command 1"), "{m}"),
            other => panic!("expected a named stop, got {other:?}"),
        }
        assert!(send(&mut p, 0x9999, &request(4, 0, &[], false, &[])).is_none());
        assert_eq!(p.cpu.x[0], 0xE401, "an unknown handle is an error, not a crash");
        p.close_handle(sm);
        assert_eq!(p.cpu.x[0], 0);
    }

    #[test]
    fn filesystem_display_gpu_audio_and_account_services_respond() {
        let mut p = process();
        let sm = connect_sm(&mut p);

        // fsp-srv: SetCurrentProcess & GetGlobalAccessLogMode
        assert!(send(&mut p, sm, &request(4, 1, b"fsp-srv\0", false, &[])).is_none());
        let fsp = reply_of(&mut p).moves[0];
        assert!(send(&mut p, fsp, &request(4, 1, &[0; 8], true, &[])).is_none());
        assert_eq!(reply_of(&mut p).result, 0);
        assert!(send(&mut p, fsp, &request(4, 1005, &[], false, &[])).is_none());
        assert_eq!(u32::from_le_bytes(reply_of(&mut p).data[..4].try_into().unwrap()), 0);

        // vi:u -> GetDisplayService -> OpenDisplay
        assert!(send(&mut p, sm, &request(4, 1, b"vi:u\0\0\0\0", false, &[])).is_none());
        let vi = reply_of(&mut p).moves[0];
        assert!(send(&mut p, vi, &request(4, 0, &0u32.to_le_bytes(), false, &[])).is_none());
        let disp = reply_of(&mut p).moves[0];
        assert!(send(&mut p, disp, &request(4, 1010, &[0; 64], false, &[])).is_none());
        assert_eq!(reply_of(&mut p).result, 0);

        // nvdrv -> Initialize & Open
        assert!(send(&mut p, sm, &request(4, 1, b"nvdrv\0\0\0", false, &[])).is_none());
        let nv = reply_of(&mut p).moves[0];
        assert!(send(&mut p, nv, &request(4, 3, &[0; 8], false, &[])).is_none());
        assert_eq!(u32::from_le_bytes(reply_of(&mut p).data[..4].try_into().unwrap()), 0);
        assert!(send(&mut p, nv, &request(4, 0, &[], false, &[])).is_none());
        assert_eq!(u32::from_le_bytes(reply_of(&mut p).data[..4].try_into().unwrap()), 1);

        // audout:u -> OpenAudioOut -> 48000 Hz stereo
        assert!(send(&mut p, sm, &request(4, 1, b"audout:u", false, &[])).is_none());
        let aud = reply_of(&mut p).moves[0];
        assert!(send(&mut p, aud, &request(4, 1, &[0; 16], false, &[])).is_none());
        let r = reply_of(&mut p);
        assert_eq!(u32::from_le_bytes(r.data[..4].try_into().unwrap()), 48_000);
        assert_eq!(r.moves.len(), 1);

        // acc:u0 -> GetUserCount -> 1
        assert!(send(&mut p, sm, &request(4, 1, b"acc:u0\0\0", false, &[])).is_none());
        let acc = reply_of(&mut p).moves[0];
        assert!(send(&mut p, acc, &request(4, 0, &[], false, &[])).is_none());
        assert_eq!(u32::from_le_bytes(reply_of(&mut p).data[..4].try_into().unwrap()), 1);

        // fsp-srv -> OpenDataStorageByCurrentProcess -> IStorage::GetSize & Read
        p.romfs = Some(b"ROMFS_TEST_BYTES".to_vec());
        assert!(send(&mut p, fsp, &request(4, 200, &[], false, &[])).is_none());
        let st = reply_of(&mut p).moves[0];
        assert!(send(&mut p, st, &request(4, 4, &[], false, &[])).is_none());
        assert_eq!(u64::from_le_bytes(reply_of(&mut p).data[..8].try_into().unwrap()), 16);
        p.cpu.x[1] = 0x20_0000;
        p.svc(0x01);
        let mut read_args = Vec::new();
        read_args.extend_from_slice(&6u64.to_le_bytes()); // offset = 6 ("TEST_BYTES")
        read_args.extend_from_slice(&10u64.to_le_bytes()); // size = 10
        assert!(send(&mut p, st, &request(4, 0, &read_args, false, &[('B', HEAP_BASE, 16)])).is_none());
        assert_eq!(p.mem.read_bytes(HEAP_BASE, 10).unwrap(), b"TEST_BYTES");
    }

    #[test]
    fn core_startup_services_answer_correctly() {
        let mut p = process();
        let sm = connect_sm(&mut p);

        // set: GetLanguageCode -> "en-US", GetRegionCode -> 1 (USA)
        assert!(send(&mut p, sm, &request(4, 1, b"set\0\0\0\0\0", false, &[])).is_none());
        let set_h = reply_of(&mut p).moves[0];
        assert!(send(&mut p, set_h, &request(4, 0, &[], false, &[])).is_none());
        assert_eq!(&reply_of(&mut p).data[..5], b"en-US");
        assert!(send(&mut p, set_h, &request(4, 4, &[], false, &[])).is_none());
        assert_eq!(u32::from_le_bytes(reply_of(&mut p).data[..4].try_into().unwrap()), 1);

        // time:u -> SystemClock::GetCurrentTime
        assert!(send(&mut p, sm, &request(4, 1, b"time:u\0\0", false, &[])).is_none());
        let time_h = reply_of(&mut p).moves[0];
        assert!(send(&mut p, time_h, &request(4, 0, &[], false, &[])).is_none());
        let clk_h = reply_of(&mut p).moves[0];
        assert!(send(&mut p, clk_h, &request(4, 0, &[], false, &[])).is_none());
        assert_eq!(i64::from_le_bytes(reply_of(&mut p).data[..8].try_into().unwrap()), 1_700_000_000);

        // apm -> OpenSession -> SetPerformanceConfiguration
        assert!(send(&mut p, sm, &request(4, 1, b"apm\0\0\0\0\0", false, &[])).is_none());
        let apm_h = reply_of(&mut p).moves[0];
        assert!(send(&mut p, apm_h, &request(4, 0, &[], false, &[])).is_none());
        let sess_h = reply_of(&mut p).moves[0];
        assert!(send(&mut p, sess_h, &request(4, 0, &[0; 8], false, &[])).is_none());
        assert_eq!(reply_of(&mut p).result, 0);

        // appletOE -> OpenApplicationProxy -> CommonStateGetter::GetFocusState & ApplicationFunctions::NotifyRunning
        assert!(send(&mut p, sm, &request(4, 1, b"appletOE", false, &[])).is_none());
        let app_h = reply_of(&mut p).moves[0];
        assert!(send(&mut p, app_h, &request(4, 0, &[0; 8], true, &[])).is_none());
        let proxy_h = reply_of(&mut p).moves[0];
        assert!(send(&mut p, proxy_h, &request(4, 0, &[], false, &[])).is_none());
        let state_h = reply_of(&mut p).moves[0];
        assert!(send(&mut p, state_h, &request(4, 9, &[], false, &[])).is_none());
        assert_eq!(reply_of(&mut p).data[0], 1);
        assert!(send(&mut p, proxy_h, &request(4, 20, &[], false, &[])).is_none());
        let fn_h = reply_of(&mut p).moves[0];
        assert!(send(&mut p, fn_h, &request(4, 40, &[], false, &[])).is_none());
        assert_eq!(reply_of(&mut p).data[0], 1);

        // hid -> ActivateNpad
        assert!(send(&mut p, sm, &request(4, 1, b"hid\0\0\0\0\0", false, &[])).is_none());
        let hid_h = reply_of(&mut p).moves[0];
        assert!(send(&mut p, hid_h, &request(4, 103, &[0; 8], true, &[])).is_none());
        assert_eq!(reply_of(&mut p).result, 0);
    }

    #[test]
    fn audout_append_buffer_decodes_stereo_pcm_and_releases_tag() {
        let mut p = process();
        p.cpu.x[1] = 0x20_0000;
        p.svc(0x01); // SetHeapSize
        let sm = connect_sm(&mut p);
        assert!(send(&mut p, sm, &request(4, 0, &[0; 8], true, &[])).is_none());
        assert!(send(&mut p, sm, &request(4, 1, b"audout:u", false, &[])).is_none());
        let root = reply_of(&mut p).moves[0];
        assert!(send(&mut p, root, &request(4, 1, &[0; 16], false, &[])).is_none());
        let aud = reply_of(&mut p).moves[0];

        // Write 2 stereo S16 frames at HEAP_BASE + 0x100 and descriptor at HEAP_BASE
        let pcm_addr = HEAP_BASE + 0x100;
        let mut pcm = Vec::new();
        pcm.extend_from_slice(&16384i16.to_le_bytes());
        pcm.extend_from_slice(&(-16384i16).to_le_bytes());
        pcm.extend_from_slice(&8192i16.to_le_bytes());
        pcm.extend_from_slice(&(-8192i16).to_le_bytes());
        assert!(p.mem.write_bytes(pcm_addr, &pcm));

        let mut desc = [0u8; 40];
        desc[8..16].copy_from_slice(&pcm_addr.to_le_bytes());
        desc[16..24].copy_from_slice(&(pcm.len() as u64).to_le_bytes());
        desc[24..32].copy_from_slice(&(pcm.len() as u64).to_le_bytes());
        assert!(p.mem.write_bytes(HEAP_BASE, &desc));

        let tag = 0x1122_3344_5566_7788u64;
        assert!(send(&mut p, aud, &request(4, 3, &tag.to_le_bytes(), false, &[('A', HEAP_BASE, 40)])).is_none());
        assert_eq!(reply_of(&mut p).result, 0);

        let mut samples = Vec::new();
        p.take_audio(&mut samples);
        assert_eq!(samples.len(), 4);
        assert!((samples[0] - 0.5).abs() < 1e-4);
        assert!((samples[1] + 0.5).abs() < 1e-4);

        let out_tags = HEAP_BASE + 0x200;
        assert!(send(&mut p, aud, &request(4, 4, &[], false, &[('B', out_tags, 16)])).is_none());
        let rep = reply_of(&mut p);
        assert_eq!(u32::from_le_bytes(rep.data[..4].try_into().unwrap()), 1);
        let got_tag = u64::from_le_bytes(p.mem.read_bytes(out_tags, 8).unwrap().try_into().unwrap());
        assert_eq!(got_tag, tag);
    }
}
