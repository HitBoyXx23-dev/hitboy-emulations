//! AES-128 with the three modes Switch content uses: ECB (key unwrapping), CTR (NCA sections)
//! and XTS (NCA headers, with Nintendo's big-endian sector tweak).

pub struct Aes128 {
    round_keys: [[u8; 16]; 11],
}

struct Tables {
    sbox: [u8; 256],
    inv: [u8; 256],
}

fn tables() -> &'static Tables {
    static T: std::sync::OnceLock<Tables> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        // Generate the S-box from its definition (inverse in GF(2^8) plus an affine map).
        let mut sbox = [0u8; 256];
        let (mut p, mut q) = (1u8, 1u8);
        loop {
            p = p ^ (p << 1) ^ if p & 0x80 != 0 { 0x1B } else { 0 };
            q ^= q << 1;
            q ^= q << 2;
            q ^= q << 4;
            if q & 0x80 != 0 {
                q ^= 0x09;
            }
            let x = q ^ q.rotate_left(1) ^ q.rotate_left(2) ^ q.rotate_left(3) ^ q.rotate_left(4);
            sbox[p as usize] = x ^ 0x63;
            if p == 1 {
                break;
            }
        }
        sbox[0] = 0x63;
        let mut inv = [0u8; 256];
        for (i, &s) in sbox.iter().enumerate() {
            inv[s as usize] = i as u8;
        }
        Tables { sbox, inv }
    })
}

fn xtime(b: u8) -> u8 {
    (b << 1) ^ if b & 0x80 != 0 { 0x1B } else { 0 }
}

fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut r = 0;
    while b != 0 {
        if b & 1 != 0 {
            r ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    r
}

impl Aes128 {
    pub fn new(key: &[u8; 16]) -> Aes128 {
        let t = tables();
        let mut w = [[0u8; 4]; 44];
        for i in 0..4 {
            w[i].copy_from_slice(&key[i * 4..i * 4 + 4]);
        }
        let mut rcon = 1u8;
        for i in 4..44 {
            let mut tmp = w[i - 1];
            if i % 4 == 0 {
                tmp.rotate_left(1);
                for b in &mut tmp {
                    *b = t.sbox[*b as usize];
                }
                tmp[0] ^= rcon;
                rcon = xtime(rcon);
            }
            for k in 0..4 {
                w[i][k] = w[i - 4][k] ^ tmp[k];
            }
        }
        let mut round_keys = [[0u8; 16]; 11];
        for (r, rk) in round_keys.iter_mut().enumerate() {
            for c in 0..4 {
                rk[c * 4..c * 4 + 4].copy_from_slice(&w[r * 4 + c]);
            }
        }
        Aes128 { round_keys }
    }

    fn add(s: &mut [u8; 16], k: &[u8; 16]) {
        for i in 0..16 {
            s[i] ^= k[i];
        }
    }

    pub fn encrypt_block(&self, block: &mut [u8; 16]) {
        let t = tables();
        Self::add(block, &self.round_keys[0]);
        for round in 1..=10 {
            for b in block.iter_mut() {
                *b = t.sbox[*b as usize];
            }
            let o = *block;
            for c in 0..4 {
                for r in 0..4 {
                    block[r + 4 * c] = o[r + 4 * ((c + r) % 4)];
                }
            }
            if round != 10 {
                for c in 0..4 {
                    let a = [block[4 * c], block[4 * c + 1], block[4 * c + 2], block[4 * c + 3]];
                    block[4 * c] = xtime(a[0]) ^ (xtime(a[1]) ^ a[1]) ^ a[2] ^ a[3];
                    block[4 * c + 1] = a[0] ^ xtime(a[1]) ^ (xtime(a[2]) ^ a[2]) ^ a[3];
                    block[4 * c + 2] = a[0] ^ a[1] ^ xtime(a[2]) ^ (xtime(a[3]) ^ a[3]);
                    block[4 * c + 3] = (xtime(a[0]) ^ a[0]) ^ a[1] ^ a[2] ^ xtime(a[3]);
                }
            }
            Self::add(block, &self.round_keys[round]);
        }
    }

    pub fn decrypt_block(&self, block: &mut [u8; 16]) {
        let t = tables();
        Self::add(block, &self.round_keys[10]);
        for round in (0..10).rev() {
            let o = *block;
            for c in 0..4 {
                for r in 0..4 {
                    block[r + 4 * ((c + r) % 4)] = o[r + 4 * c];
                }
            }
            for b in block.iter_mut() {
                *b = t.inv[*b as usize];
            }
            Self::add(block, &self.round_keys[round]);
            if round != 0 {
                for c in 0..4 {
                    let a = [block[4 * c], block[4 * c + 1], block[4 * c + 2], block[4 * c + 3]];
                    block[4 * c] = gmul(a[0], 14) ^ gmul(a[1], 11) ^ gmul(a[2], 13) ^ gmul(a[3], 9);
                    block[4 * c + 1] = gmul(a[0], 9) ^ gmul(a[1], 14) ^ gmul(a[2], 11) ^ gmul(a[3], 13);
                    block[4 * c + 2] = gmul(a[0], 13) ^ gmul(a[1], 9) ^ gmul(a[2], 14) ^ gmul(a[3], 11);
                    block[4 * c + 3] = gmul(a[0], 11) ^ gmul(a[1], 13) ^ gmul(a[2], 9) ^ gmul(a[3], 14);
                }
            }
        }
    }
}

pub fn key16(b: &[u8]) -> Option<[u8; 16]> {
    b.get(..16)?.try_into().ok()
}

/// Decrypts one 16-byte block (key unwrapping).
pub fn ecb_decrypt_block(key: &[u8; 16], block: &[u8; 16]) -> [u8; 16] {
    let mut b = *block;
    Aes128::new(key).decrypt_block(&mut b);
    b
}

/// CTR keystream XOR starting at `counter` (a 16-byte big-endian counter). Same for both directions.
pub fn ctr_xor(key: &[u8; 16], counter: &[u8; 16], data: &mut [u8]) {
    let aes = Aes128::new(key);
    let mut ctr = u128::from_be_bytes(*counter);
    for chunk in data.chunks_mut(16) {
        let mut ks = ctr.to_be_bytes();
        aes.encrypt_block(&mut ks);
        for (d, k) in chunk.iter_mut().zip(ks) {
            *d ^= k;
        }
        ctr = ctr.wrapping_add(1);
    }
}

fn xts_mul(t: &mut [u8; 16]) {
    let carry = t[15] >> 7;
    for i in (1..16).rev() {
        t[i] = (t[i] << 1) | (t[i - 1] >> 7);
    }
    t[0] = (t[0] << 1) ^ if carry != 0 { 0x87 } else { 0 };
}

fn xts(key: &[u8; 32], first_sector: u64, sector_size: usize, data: &mut [u8], decrypt: bool) {
    let k1 = Aes128::new(key[..16].try_into().unwrap());
    let k2 = Aes128::new(key[16..].try_into().unwrap());
    for (n, sector) in data.chunks_mut(sector_size).enumerate() {
        // Nintendo's tweak is the sector number big-endian (standard XTS is little-endian).
        let mut tweak = ((first_sector + n as u64) as u128).to_be_bytes();
        k2.encrypt_block(&mut tweak);
        for block in sector.chunks_exact_mut(16) {
            let mut b: [u8; 16] = (&*block).try_into().unwrap();
            for i in 0..16 {
                b[i] ^= tweak[i];
            }
            if decrypt {
                k1.decrypt_block(&mut b);
            } else {
                k1.encrypt_block(&mut b);
            }
            for i in 0..16 {
                block[i] = b[i] ^ tweak[i];
            }
            xts_mul(&mut tweak);
        }
    }
}

pub fn xts_decrypt(key: &[u8; 32], first_sector: u64, sector_size: usize, data: &mut [u8]) {
    xts(key, first_sector, sector_size, data, true)
}

pub fn xts_encrypt(key: &[u8; 32], first_sector: u64, sector_size: usize, data: &mut [u8]) {
    xts(key, first_sector, sector_size, data, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn fips_197_block() {
        let key: [u8; 16] = std::array::from_fn(|i| i as u8);
        let mut b: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().unwrap();
        let aes = Aes128::new(&key);
        aes.encrypt_block(&mut b);
        assert_eq!(b.to_vec(), hex("69c4e0d86a7b0430d8cdb78070b4c55a"));
        aes.decrypt_block(&mut b);
        assert_eq!(b.to_vec(), hex("00112233445566778899aabbccddeeff"));
    }

    #[test]
    fn nist_ctr() {
        let key: [u8; 16] = hex("2b7e151628aed2a6abf7158809cf4f3c").try_into().unwrap();
        let ctr: [u8; 16] = hex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff").try_into().unwrap();
        let mut d = hex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51");
        ctr_xor(&key, &ctr, &mut d);
        assert_eq!(d, hex("874d6191b620e3261bef6864990db6ce9806f66b7970fdff8617187bb9fffdff"));
    }

    #[test]
    fn ieee_1619_xts_vector_1() {
        let mut d = vec![0u8; 32];
        xts_encrypt(&[0u8; 32], 0, 0x200, &mut d);
        assert_eq!(d, hex("917cf69ebd68b2ec9b9fe9a3eadda692cd43d2f59598ed858c02c2652fbf922e"));
        xts_decrypt(&[0u8; 32], 0, 0x200, &mut d);
        assert_eq!(d, vec![0u8; 32]);
    }

    #[test]
    fn xts_sectors_differ() {
        let key: [u8; 32] = std::array::from_fn(|i| i as u8 * 7 + 1);
        let mut d = vec![0x55u8; 0x400];
        xts_encrypt(&key, 0, 0x200, &mut d);
        assert_ne!(d[..0x200], d[0x200..]);
        xts_decrypt(&key, 0, 0x200, &mut d);
        assert!(d.iter().all(|&b| b == 0x55));
    }
}
