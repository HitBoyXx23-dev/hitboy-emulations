//! Apple IMG4 / IM4P container unwrapper and `complzss` (LZSS) kernelcache decompressor.
//!
//! Starting with iOS 10, Apple stopped encrypting kernelcaches inside `.ipsw` files.
//! Modern `.ipsw` kernelcaches are ASN.1 DER `IM4P` containers wrapping an LZSS (`complzss`)
//! or LZFSE (`bvx2`) compressed XNU Mach-O kernel.

#[derive(Debug, Clone)]
pub struct Im4p {
    pub fourcc: String,
    pub description: String,
    pub payload: Vec<u8>,
}

/// Reads one ASN.1 DER TLV header `(tag, content_slice, next_offset)`.
fn read_der_tlv(data: &[u8], off: usize) -> Result<(u8, &[u8], usize), String> {
    let tag = *data.get(off).ok_or("DER truncated at tag")?;
    let len_byte = *data.get(off + 1).ok_or("DER truncated at length")?;
    let (len, hdr_end) = if len_byte & 0x80 == 0 {
        (len_byte as usize, off + 2)
    } else {
        let n = (len_byte & 0x7F) as usize;
        if n == 0 || n > 4 {
            return Err("Unsupported DER length encoding.".into());
        }
        let mut len = 0usize;
        for i in 0..n {
            let b = *data.get(off + 2 + i).ok_or("DER truncated in length bytes")?;
            len = (len << 8) | b as usize;
        }
        (len, off + 2 + n)
    };
    let end = hdr_end.checked_add(len).ok_or("DER length overflow")?;
    let body = data.get(hdr_end..end).ok_or("DER value runs past end of buffer")?;
    Ok((tag, body, end))
}

/// Unwraps an Apple `IM4P` (or `IMG4` containing `IM4P`) ASN.1 DER container.
pub fn parse_im4p(data: &[u8]) -> Result<Im4p, String> {
    let (tag, mut seq, _) = read_der_tlv(data, 0)?;
    if tag != 0x30 {
        return Err("Not an ASN.1 DER SEQUENCE (not IM4P/IMG4).".into());
    }
    let (t0, v0, mut pos) = read_der_tlv(seq, 0)?;
    if t0 != 0x16 {
        return Err("Expected IA5String magic in IM4P/IMG4.".into());
    }
    if v0 == b"IMG4" {
        // IMG4 wraps an inner IM4P SEQUENCE as its second element.
        let (inner_tag, inner_body, _) = read_der_tlv(seq, pos)?;
        if inner_tag != 0x30 {
            return Err("IMG4 does not contain an inner IM4P SEQUENCE.".into());
        }
        seq = inner_body;
        let (it0, iv0, ipos) = read_der_tlv(seq, 0)?;
        if it0 != 0x16 || iv0 != b"IM4P" {
            return Err("IMG4 inner element is not IM4P.".into());
        }
        pos = ipos;
    } else if v0 != b"IM4P" {
        return Err(format!("Expected IM4P magic, found {:?}.", String::from_utf8_lossy(v0)));
    }

    let (t1, v1, pos) = read_der_tlv(seq, pos)?;
    if t1 != 0x16 {
        return Err("Expected IA5String type fourcc in IM4P.".into());
    }
    let (t2, v2, pos) = read_der_tlv(seq, pos)?;
    if t2 != 0x16 {
        return Err("Expected IA5String description in IM4P.".into());
    }
    let (t3, v3, _) = read_der_tlv(seq, pos)?;
    if t3 != 0x04 {
        return Err("Expected OCTET STRING payload in IM4P.".into());
    }

    Ok(Im4p {
        fourcc: String::from_utf8_lossy(v1).into_owned(),
        description: String::from_utf8_lossy(v2).into_owned(),
        payload: v3.to_vec(),
    })
}

/// Decompresses an Apple `complzss` kernelcache payload (0x180-byte header + LZSS stream).
pub fn decompress_complzss(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 0x180 || &data[..8] != b"complzss" {
        return Err("Not an Apple complzss payload.".into());
    }
    let uncomp_size = u32::from_be_bytes(data[12..16].try_into().unwrap()) as usize;
    let comp_size = u32::from_be_bytes(data[16..20].try_into().unwrap()) as usize;
    if uncomp_size > 256 << 20 {
        return Err(format!("complzss uncompressed size ({uncomp_size} bytes) exceeds 256 MiB safety limit."));
    }
    let src_end = (0x180 + comp_size).min(data.len());
    let src = &data[0x180..src_end];

    const N: usize = 4096;
    const F: usize = 18;
    const THRESHOLD: usize = 2;

    let mut ring = [0x20u8; N];
    let mut r = N - F;
    let mut out = Vec::with_capacity(uncomp_size);
    let mut i = 0usize;
    let mut flags: u32 = 0;

    while i < src.len() && out.len() < uncomp_size {
        flags >>= 1;
        if (flags & 0x100) == 0 {
            flags = src[i] as u32 | 0xFF00;
            i += 1;
            if i >= src.len() {
                break;
            }
        }
        if (flags & 1) != 0 {
            let c = src[i];
            i += 1;
            out.push(c);
            ring[r] = c;
            r = (r + 1) & (N - 1);
        } else {
            if i + 1 >= src.len() {
                break;
            }
            let b0 = src[i] as usize;
            let b1 = src[i + 1] as usize;
            i += 2;
            let pos = b0 | ((b1 & 0xF0) << 4);
            let len = (b1 & 0x0F) + THRESHOLD + 1;
            for k in 0..len {
                if out.len() >= uncomp_size {
                    break;
                }
                let c = ring[(pos + k) & (N - 1)];
                out.push(c);
                ring[r] = c;
                r = (r + 1) & (N - 1);
            }
        }
    }
    Ok(out)
}

/// Decompresses an Apple LZFSE (`bvx2` / `bvx1` / `bvxn` / `bvx-`) kernelcache payload.
pub fn decompress_lzfse(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 4 || !matches!(&data[..4], b"bvx2" | b"bvx1" | b"bvxn" | b"bvx-") {
        return Err("Not an Apple LZFSE (bvx*) payload.".into());
    }
    let mut out = Vec::new();
    lzfse_rust::decode_bytes(data, &mut out).map_err(|e| format!("LZFSE decompression failed: {e}"))?;
    Ok(out)
}

/// Extracts the `"Darwin Kernel Version ..."` banner string from a decompressed XNU kernel, if present.
pub fn extract_xnu_banner(kernel: &[u8]) -> Option<String> {
    let needle = b"Darwin Kernel Version ";
    let pos = kernel.windows(needle.len()).position(|w| w == needle)?;
    let slice = &kernel[pos..(pos + 256).min(kernel.len())];
    let end = slice.iter().position(|&b| b == 0 || b == b'\n').unwrap_or(slice.len());
    Some(String::from_utf8_lossy(&slice[..end]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let n = body.len();
        if n < 0x80 {
            out.push(n as u8);
        } else if n < 0x100 {
            out.push(0x81);
            out.push(n as u8);
        } else {
            out.push(0x82);
            out.push((n >> 8) as u8);
            out.push((n & 0xFF) as u8);
        }
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn unwraps_im4p_and_decompresses_complzss() {
        // Build a valid complzss stream containing literal bytes "Darwin Kernel Version 23.0.0"
        let msg = b"Darwin Kernel Version 23.0.0: ARM64";
        let mut comp = Vec::new();
        for chunk in msg.chunks(8) {
            comp.push(0xFFu8); // all 8 bits = literal
            comp.extend_from_slice(chunk);
        }
        let mut lzss = vec![0u8; 0x180];
        lzss[..8].copy_from_slice(b"complzss");
        lzss[12..16].copy_from_slice(&(msg.len() as u32).to_be_bytes());
        lzss[16..20].copy_from_slice(&(comp.len() as u32).to_be_bytes());
        lzss[20..24].copy_from_slice(&1u32.to_be_bytes());
        lzss.extend_from_slice(&comp);

        let mut seq_body = Vec::new();
        seq_body.extend(der_tlv(0x16, b"IM4P"));
        seq_body.extend(der_tlv(0x16, b"krnl"));
        seq_body.extend(der_tlv(0x16, b"KernelCacheBuilder"));
        seq_body.extend(der_tlv(0x04, &lzss));
        let im4p_bytes = der_tlv(0x30, &seq_body);

        let im4p = parse_im4p(&im4p_bytes).expect("valid IM4P");
        assert_eq!(im4p.fourcc, "krnl");
        let raw = decompress_complzss(&im4p.payload).expect("valid complzss");
        assert_eq!(raw, msg);
        assert_eq!(extract_xnu_banner(&raw).as_deref(), Some("Darwin Kernel Version 23.0.0: ARM64"));
    }
}
