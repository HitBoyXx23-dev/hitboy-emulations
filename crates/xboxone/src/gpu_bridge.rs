//! Xbox One / Series X|S graphics bridge: DXBC/DXIL shader container parsing and console-chunk
//! sanitization, D3D12.X root-signature flag normalization, XG texture surface layout calculations,
//! and an offscreen RGBA framebuffer for Emulator mode presentation.

/// Standard desktop D3D12_ROOT_SIGNATURE_FLAGS bits (through Direct3D 12 Agility SDK).
pub const DESKTOP_ROOT_SIG_FLAG_MASK: u32 = 0x0000_0FFF;

/// Strips Xbox-only D3D12.X / D3D12.XS root signature flags (such as console-specific upper bits
/// that desktop D3D12SerializeRootSignature rejects with E_INVALIDARG) and returns
/// `(normalized_flags, stripped_bits)`.
pub fn sanitize_root_signature_flags(flags: u32) -> (u32, u32) {
    let normalized = flags & DESKTOP_ROOT_SIG_FLAG_MASK;
    let stripped = flags & !DESKTOP_ROOT_SIG_FLAG_MASK;
    (normalized, stripped)
}

/// One FourCC chunk inside a DXBC shader container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DxbcChunk {
    pub fourcc: [u8; 4],
    pub data: Vec<u8>,
}

impl DxbcChunk {
    pub fn name(&self) -> String {
        String::from_utf8_lossy(&self.fourcc).into_owned()
    }
}

/// Parsed DXBC / DXIL shader container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DxbcContainer {
    pub digest: [u8; 16],
    pub version: u32,
    pub chunks: Vec<DxbcChunk>,
}

impl DxbcContainer {
    /// Parses a DXBC container starting with magic `b"DXBC"`.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 32 || &bytes[0..4] != b"DXBC" {
            return Err("not a DXBC shader container".into());
        }
        let mut digest = [0u8; 16];
        digest.copy_from_slice(&bytes[4..20]);
        let version = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
        let declared_size = u32::from_le_bytes(bytes[24..28].try_into().unwrap()) as usize;
        let chunk_count = u32::from_le_bytes(bytes[28..32].try_into().unwrap()) as usize;
        if declared_size > bytes.len() || chunk_count > 64 {
            return Err("DXBC container header is truncated or corrupt".into());
        }
        let table_end = 32usize.checked_add(chunk_count * 4).ok_or("DXBC chunk table overflow")?;
        if table_end > declared_size {
            return Err("DXBC chunk offset table exceeds container size".into());
        }
        let mut chunks = Vec::with_capacity(chunk_count);
        for i in 0..chunk_count {
            let off = u32::from_le_bytes(bytes[32 + i * 4..36 + i * 4].try_into().unwrap()) as usize;
            if off.checked_add(8).map_or(true, |e| e > declared_size) {
                return Err(format!("DXBC chunk {i} offset {off:#x} is out of bounds"));
            }
            let fourcc: [u8; 4] = bytes[off..off + 4].try_into().unwrap();
            let len = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
            let end = off.checked_add(8).and_then(|s| s.checked_add(len)).ok_or("DXBC chunk length overflow")?;
            if end > declared_size {
                return Err(format!("DXBC chunk {i} ({}) exceeds container bounds", String::from_utf8_lossy(&fourcc)));
            }
            chunks.push(DxbcChunk {
                fourcc,
                data: bytes[off + 8..end].to_vec(),
            });
        }
        Ok(Self { digest, version, chunks })
    }

    pub fn has_chunk(&self, fourcc: &[u8; 4]) -> bool {
        self.chunks.iter().any(|c| &c.fourcc == fourcc)
    }

    /// True if any chunk carries console-specific binary shader payload (`XDAT` / `XBOX` / `SCAR`).
    pub fn has_console_private_chunks(&self) -> bool {
        self.chunks.iter().any(|c| {
            matches!(&c.fourcc, b"XDAT" | b"XBOX" | b"SCAR")
                || (&c.fourcc == b"DXIL" && c.data.windows(4).any(|w| w == b"XDAT"))
        })
    }

    /// Serializes a sanitized container keeping only desktop-compatible chunks (dropping vendor-only
    /// `XDAT`/`XBOX`/`SCAR`/`PRIV` chunks) with fresh chunk offsets.
    pub fn serialize_desktop(&self) -> Vec<u8> {
        let kept: Vec<&DxbcChunk> = self
            .chunks
            .iter()
            .filter(|c| !matches!(&c.fourcc, b"XDAT" | b"XBOX" | b"SCAR" | b"PRIV"))
            .collect();
        let header_size = 32 + kept.len() * 4;
        let mut offsets = Vec::with_capacity(kept.len());
        let mut cursor = header_size;
        for c in &kept {
            offsets.push(cursor as u32);
            cursor += 8 + c.data.len();
        }
        let mut out = Vec::with_capacity(cursor);
        out.extend_from_slice(b"DXBC");
        out.extend_from_slice(&[0u8; 16]); // unsigned desktop container (hash cleared for re-signing/DXIL)
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&(cursor as u32).to_le_bytes());
        out.extend_from_slice(&(kept.len() as u32).to_le_bytes());
        for off in offsets {
            out.extend_from_slice(&off.to_le_bytes());
        }
        for c in kept {
            out.extend_from_slice(&c.fourcc);
            out.extend_from_slice(&(c.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&c.data);
        }
        out
    }
}

/// Xbox XG texture tiling modes (simplified clean-room layout taxonomy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum XgTileMode {
    Linear = 0,
    Display2D = 1,
    Thin2D = 2,
    Depth2D = 3,
    Volume3D = 4,
}

/// Surface format descriptors needed for XG pitch and size calculations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XgFormat {
    R8G8B8A8Unorm,
    B8G8R8A8Unorm,
    R16G16B16A16Float,
    R32Float,
    D32Float,
    Bc1Unorm,
    Bc3Unorm,
    Bc7Unorm,
}

impl XgFormat {
    pub fn bytes_per_block(self) -> u32 {
        match self {
            Self::R8G8B8A8Unorm | Self::B8G8R8A8Unorm | Self::R32Float | Self::D32Float => 4,
            Self::R16G16B16A16Float | Self::Bc1Unorm => 8,
            Self::Bc3Unorm | Self::Bc7Unorm => 16,
        }
    }

    pub fn block_dim(self) -> u32 {
        match self {
            Self::Bc1Unorm | Self::Bc3Unorm | Self::Bc7Unorm => 4,
            _ => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XgMipLevel {
    pub width: u32,
    pub height: u32,
    pub row_pitch_bytes: u32,
    pub slice_bytes: u64,
    pub offset_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XgTextureLayout {
    pub tile_mode: XgTileMode,
    pub alignment_bytes: u64,
    pub total_bytes: u64,
    pub mips: Vec<XgMipLevel>,
}

pub fn compute_optimal_tile_mode(format: XgFormat, is_render_target: bool, is_depth: bool) -> XgTileMode {
    if is_depth || matches!(format, XgFormat::D32Float) {
        XgTileMode::Depth2D
    } else if is_render_target {
        XgTileMode::Display2D
    } else {
        XgTileMode::Thin2D
    }
}

/// Computes D3D12-compatible aligned mip chain layout for a 2D texture (256-byte row pitch alignment,
/// 64 KiB resource alignment for tiled surfaces).
pub fn compute_texture2d_layout(width: u32, height: u32, mip_levels: u32, format: XgFormat, tile_mode: XgTileMode) -> XgTextureLayout {
    let block = format.block_dim();
    let bpb = format.bytes_per_block();
    let levels = mip_levels.max(1);
    let mut mips = Vec::with_capacity(levels as usize);
    let mut offset = 0u64;
    let mut w = width.max(1);
    let mut h = height.max(1);
    let row_align = 256u32; // D3D12_TEXTURE_DATA_PITCH_ALIGNMENT
    let subresource_align = 512u64; // D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT

    for _ in 0..levels {
        let blocks_w = w.div_ceil(block);
        let blocks_h = h.div_ceil(block);
        let raw_row = blocks_w.saturating_mul(bpb);
        let row_pitch = raw_row.div_ceil(row_align) * row_align;
        let slice_bytes = (row_pitch as u64) * (blocks_h as u64);
        mips.push(XgMipLevel {
            width: w,
            height: h,
            row_pitch_bytes: row_pitch,
            slice_bytes,
            offset_bytes: offset,
        });
        offset = (offset + slice_bytes).div_ceil(subresource_align) * subresource_align;
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }

    let alignment_bytes = match tile_mode {
        XgTileMode::Linear => 4096,
        _ => 65536,
    };
    let total_bytes = offset.max(alignment_bytes).div_ceil(alignment_bytes) * alignment_bytes;
    XgTextureLayout {
        tile_mode,
        alignment_bytes,
        total_bytes,
        mips,
    }
}

/// Offscreen RGBA8 framebuffer used by Emulator mode and the Xbox graphics bridge.
#[derive(Debug, Clone)]
pub struct XboxFramebuffer {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub presents: u64,
}

impl XboxFramebuffer {
    pub fn new(width: u32, height: u32) -> Self {
        let len = (width as usize).saturating_mul(height as usize).saturating_mul(4);
        Self {
            width,
            height,
            rgba: vec![0u8; len],
            presents: 0,
        }
    }

    pub fn clear(&mut self, r: u8, g: u8, b: u8, a: u8) {
        for px in self.rgba.chunks_exact_mut(4) {
            px[0] = r;
            px[1] = g;
            px[2] = b;
            px[3] = a;
        }
    }

    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
        let x1 = x.saturating_add(w).min(self.width);
        let y1 = y.saturating_add(h).min(self.height);
        for py in y.min(self.height)..y1 {
            for px in x.min(self.width)..x1 {
                let idx = ((py * self.width + px) * 4) as usize;
                self.rgba[idx..idx + 4].copy_from_slice(&color);
            }
        }
    }

    pub fn present(&mut self) -> u64 {
        self.presents += 1;
        self.presents
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_dxbc(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let c = DxbcContainer {
            digest: [0; 16],
            version: 1,
            chunks: chunks
                .iter()
                .map(|(f, d)| DxbcChunk { fourcc: **f, data: d.to_vec() })
                .collect(),
        };
        let header_size = 32 + c.chunks.len() * 4;
        let mut cur = header_size;
        let mut offs = Vec::new();
        for ch in &c.chunks {
            offs.push(cur as u32);
            cur += 8 + ch.data.len();
        }
        let mut out = Vec::with_capacity(cur);
        out.extend_from_slice(b"DXBC");
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(cur as u32).to_le_bytes());
        out.extend_from_slice(&(c.chunks.len() as u32).to_le_bytes());
        for o in offs {
            out.extend_from_slice(&o.to_le_bytes());
        }
        for ch in &c.chunks {
            out.extend_from_slice(&ch.fourcc);
            out.extend_from_slice(&(ch.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&ch.data);
        }
        out
    }

    #[test]
    fn root_signature_flag_sanitizer_strips_xbox_high_bits() {
        let (norm, stripped) = sanitize_root_signature_flags(0x0300_0001);
        assert_eq!(norm, 0x0000_0001);
        assert_eq!(stripped, 0x0300_0000);
    }

    #[test]
    fn dxbc_container_parses_and_strips_console_chunks() {
        let raw = build_dxbc(&[(b"ISGN", &[1, 2, 3, 4]), (b"XDAT", &[9, 9, 9]), (b"DXIL", &[5, 6, 7, 8])]);
        let parsed = DxbcContainer::parse(&raw).unwrap();
        assert_eq!(parsed.chunks.len(), 3);
        assert!(parsed.has_console_private_chunks());

        let clean_bytes = parsed.serialize_desktop();
        let clean = DxbcContainer::parse(&clean_bytes).unwrap();
        assert_eq!(clean.chunks.len(), 2);
        assert!(!clean.has_console_private_chunks());
        assert!(clean.has_chunk(b"ISGN"));
        assert!(clean.has_chunk(b"DXIL"));
    }

    #[test]
    fn xg_texture_layout_handles_bc7_mips() {
        let mode = compute_optimal_tile_mode(XgFormat::Bc7Unorm, false, false);
        assert_eq!(mode, XgTileMode::Thin2D);
        let layout = compute_texture2d_layout(256, 256, 4, XgFormat::Bc7Unorm, mode);
        assert_eq!(layout.mips.len(), 4);
        assert_eq!(layout.mips[0].row_pitch_bytes, 1024);
        assert_eq!(layout.alignment_bytes, 65536);
        assert!(layout.total_bytes >= 65536);
    }

    #[test]
    fn framebuffer_clear_rect_and_present() {
        let mut fb = XboxFramebuffer::new(64, 64);
        fb.clear(10, 20, 30, 255);
        fb.fill_rect(4, 4, 8, 8, [255, 0, 128, 255]);
        assert_eq!(&fb.rgba[0..4], &[10, 20, 30, 255]);
        let idx = ((4 * 64 + 4) * 4) as usize;
        assert_eq!(&fb.rgba[idx..idx + 4], &[255, 0, 128, 255]);
        assert_eq!(fb.present(), 1);
    }
}
