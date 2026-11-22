use super::*;

pub(super) const UNIX_EPOCH_TICKS: u64 = 62135596800000000;
const DAY_TICKS: u64 = 86400000000;
const BAD_POINTER: u64 = 0x80B50002;
const BAD_VALUE: u64 = 0x80B50003;

pub(super) fn date_from_ticks(ticks: u64) -> Option<[u8; 16]> {
    let mut days = ticks / DAY_TICKS;
    if days >= 3652059 { return None; }
    let mut year = days / 146097 * 400 + 1;
    days %= 146097;
    let leap = |year: u64| year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    loop {
        let count = if leap(year) { 366 } else { 365 };
        if days < count { break; }
        days -= count;
        year += 1;
    }
    let months = [31, if leap(year) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0;
    while days >= months[month] { days -= months[month]; month += 1; }
    let seconds = ticks % DAY_TICKS / 1000000;
    let fields = [year as u16, month as u16 + 1, days as u16 + 1, (seconds / 3600) as u16, (seconds / 60 % 60) as u16, (seconds % 60) as u16];
    let mut bytes = [0; 16];
    for (index, field) in fields.into_iter().enumerate() { bytes[index * 2..index * 2 + 2].copy_from_slice(&field.to_le_bytes()); }
    bytes[12..16].copy_from_slice(&((ticks % 1000000) as u32).to_le_bytes());
    Some(bytes)
}

impl Machine {
    fn current_rtc_tick(&mut self) -> u64 {
        let output = self.arg(0);
        if output == 0 || !self.mem.read(output, &mut [0; 8]) { return BAD_POINTER; }
        let ticks = UNIX_EPOCH_TICKS + 1700000000 * 1000000 + self.cpu.tsc / 1000;
        if self.mem.write_u64(output, ticks) { 0 } else { BAD_POINTER }
    }

    fn rtc_tick_to_calendar(&mut self) -> u64 {
        let (output, input) = (self.arg(0), self.arg(1));
        let mut tick = [0; 8];
        if input == 0 || output == 0 || !self.mem.read(input, &mut tick) || !self.mem.read(output, &mut [0; 16]) { return BAD_POINTER; }
        let Some(date) = date_from_ticks(u64::from_le_bytes(tick)) else { return BAD_VALUE; };
        if self.mem.write(output, &date) { 0 } else { BAD_POINTER }
    }

    pub(super) fn call_rtc(&mut self, name: &str) -> bool {
        let result = match name {
            "sceRtcGetCurrentTick" => self.current_rtc_tick(),
            "sceRtcSetTick" => self.rtc_tick_to_calendar(),
            "sceRtcGetTickResolution" => 1000000,
            _ => return false,
        };
        self.ret(result);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtc_ticks_use_the_native_epoch_and_preserve_calendar_guards() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let storage = machine.heap_alloc(48, 16);
        machine.mem.write(storage, &[0xCC; 48]);
        machine.cpu.tsc = 1234567890;
        machine.cpu.r[7] = storage + 32;
        assert!(machine.call_rtc("sceRtcGetCurrentTick"));
        assert_eq!(machine.cpu.r[0], 0);
        let ticks = machine.mem.read_u64(storage + 32);
        assert_eq!(ticks, UNIX_EPOCH_TICKS + 1700000000 * 1000000 + 1234567);
        machine.cpu.r[7] = storage + 8;
        machine.cpu.r[6] = storage + 32;
        machine.call_rtc("sceRtcSetTick");
        assert_eq!(machine.cpu.r[0], 0);
        let mut date = [0; 16];
        machine.mem.read(storage + 8, &mut date);
        assert_eq!(date, clock_bytes(machine.cpu.tsc));
        assert_eq!(date, [0xE7, 7, 11, 0, 14, 0, 22, 0, 13, 0, 21, 0, 0x47, 0x94, 3, 0]);
        assert_eq!(machine.mem.read_u64(storage), 0xCCCCCCCCCCCCCCCC);
        assert_eq!(machine.mem.read_u64(storage + 24), 0xCCCCCCCCCCCCCCCC);
        for (ticks, expected) in [(0, [1, 1, 1]), (UNIX_EPOCH_TICKS, [1970, 1, 1]), (UNIX_EPOCH_TICKS + 951782400 * 1000000, [2000, 2, 29]), (UNIX_EPOCH_TICKS - 2203891200 * 1000000, [1900, 3, 1])] {
            let date = date_from_ticks(ticks).unwrap();
            for index in 0..3 { assert_eq!(u16::from_le_bytes(date[index * 2..index * 2 + 2].try_into().unwrap()), expected[index]); }
        }
        machine.mem.write_u64(storage + 32, u64::MAX);
        machine.call_rtc("sceRtcSetTick");
        assert_eq!(machine.cpu.r[0], BAD_VALUE);
        let mut unchanged = [0; 16];
        machine.mem.read(storage + 8, &mut unchanged);
        assert_eq!(unchanged, date);
        machine.cpu.r[6] = u64::MAX;
        machine.call_rtc("sceRtcSetTick");
        assert_eq!(machine.cpu.r[0], BAD_POINTER);
    }
}
