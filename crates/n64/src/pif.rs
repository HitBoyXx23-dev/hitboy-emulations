//! The PIF's joybus: the 64-byte command RAM through which games talk to the four
//! controller ports and the cartridge's serial EEPROM.

/// Cartridge EEPROM size in bytes from the game code in the header (0 = none reported;
/// the 4 Kbit chip is assumed, and a few known games use 16 Kbit).
pub fn detect_eeprom(rom: &[u8]) -> usize {
    let code = rom.get(0x3C..0x3E).map(|c| [c[0], c[1]]).unwrap_or([0, 0]);
    const BIG: [[u8; 2]; 7] = [*b"YS", *b"B7", *b"PD", *b"DO", *b"MX", *b"EP", *b"GU"];
    if BIG.contains(&code) {
        2048
    } else {
        512
    }
}

pub struct Pif {
    pub ram: [u8; 64],
    pub eeprom: Vec<u8>,
    buttons: [u16; 4],
    stick: [(i8, i8); 4],
    pub connected: [bool; 4],
}

impl Pif {
    pub fn new(eeprom_size: usize) -> Pif {
        Pif { ram: [0; 64], eeprom: vec![0xFF; eeprom_size], buttons: [0; 4], stick: [(0, 0); 4], connected: [true, false, false, false] }
    }

    pub fn set_input(&mut self, pad: usize, buttons: u16, x: i8, y: i8) {
        if pad < 4 {
            self.buttons[pad] = buttons;
            self.stick[pad] = (x, y);
            if buttons != 0 {
                self.connected[pad] = true;
            }
        }
    }

    /// Runs every command block in PIF RAM, writing the responses in place.
    pub fn run_commands(&mut self) {
        let mut i = 0usize;
        let mut ch = 0usize;
        while i < 63 && ch < 6 {
            let tx = self.ram[i];
            match tx {
                0xFE => break,
                0xFF | 0xFD => {
                    i += 1;
                    continue;
                }
                0 => {
                    ch += 1;
                    i += 1;
                    continue;
                }
                _ => {}
            }
            let txn = (tx & 0x3F) as usize;
            let rxn = (self.ram[i + 1] & 0x3F) as usize;
            let cmd_at = i + 2;
            let resp_at = cmd_at + txn;
            if resp_at + rxn > 64 {
                break;
            }
            let cmd: Vec<u8> = self.ram[cmd_at..resp_at].to_vec();
            let resp = self.device(ch, &cmd);
            match resp {
                Some(r) => {
                    for (k, b) in r.into_iter().take(rxn).enumerate() {
                        self.ram[resp_at + k] = b;
                    }
                }
                None => self.ram[i + 1] |= 0x80, // no device on this channel
            }
            i = resp_at + rxn;
            ch += 1;
        }
        self.ram[63] = 0;
    }

    fn device(&mut self, ch: usize, cmd: &[u8]) -> Option<Vec<u8>> {
        let c = *cmd.first()?;
        if ch < 4 {
            if !self.connected[ch] {
                return None;
            }
            return Some(match c {
                0x00 | 0xFF => vec![0x05, 0x00, 0x02],
                0x01 => {
                    let b = self.buttons[ch];
                    let (x, y) = self.stick[ch];
                    vec![(b >> 8) as u8, b as u8, x as u8, y as u8]
                }
                // Controller Pak: none inserted.
                0x02 => vec![0; 33],
                0x03 => vec![0],
                _ => return None,
            });
        }
        if ch == 4 {
            let big = self.eeprom.len() > 512;
            return Some(match c {
                0x00 | 0xFF => vec![0x00, if big { 0xC0 } else { 0x80 }, 0x00],
                0x04 => {
                    let blk = *cmd.get(1)? as usize * 8;
                    (0..8).map(|k| *self.eeprom.get(blk + k).unwrap_or(&0)).collect()
                }
                0x05 => {
                    let blk = *cmd.get(1)? as usize * 8;
                    for k in 0..8 {
                        if let (Some(d), Some(&s)) = (self.eeprom.get_mut(blk + k), cmd.get(2 + k)) {
                            *d = s;
                        }
                    }
                    vec![0]
                }
                _ => return None,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_controller_and_eeprom() {
        let mut p = Pif::new(512);
        p.set_input(0, 0x8000, 40, -20);
        // Channel 0: read buttons (tx 1, rx 4). Channel 1: skip. Channel 4: EEPROM write then read.
        let mut ram = [0u8; 64];
        let prog: &[u8] = &[0x01, 0x04, 0x01, 0, 0, 0, 0, 0x00, 0x00, 0x00, 0x0A, 0x01, 0x05, 3, 1, 2, 3, 4, 5, 6, 7, 8, 0, 0xFE];
        ram[..prog.len()].copy_from_slice(prog);
        p.ram = ram;
        p.run_commands();
        assert_eq!(&p.ram[3..7], &[0x80, 0x00, 40, (-20i8) as u8]);
        assert_eq!(&p.eeprom[24..32], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
