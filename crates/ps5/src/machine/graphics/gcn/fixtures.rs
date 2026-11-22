use super::*;
use std::path::PathBuf;

pub(crate) struct RasterSnapshot {
    pub root: PathBuf,
    pub mem: Mem,
    pub registers: BTreeMap<u32, u32>,
    pub indices: Vec<u32>,
    pub vertex_code: Vec<u8>,
}

pub(crate) fn raster_snapshot() -> RasterSnapshot {
    let root = PathBuf::from(std::env::var_os("HITBOY_PS4_RASTER_SNAPSHOT").expect("Supply the private indexed-draw snapshot directory"));
    assert!(root.join("raster-vertex-s2-table-buffer-0-record-0.bin").is_file(), "The snapshot must include actual indexed vertex records");
    let bytes = std::fs::read(root.join("graphics-registers.bin")).unwrap();
    let registers = bytes.chunks_exact(8).map(|pair| (u32::from_le_bytes(pair[..4].try_into().unwrap()), u32::from_le_bytes(pair[4..].try_into().unwrap()))).collect::<BTreeMap<_, _>>();
    let mut mem = Mem::new().unwrap();
    let vertex_code = std::fs::read(root.join("vertex-shader-prefix.bin")).unwrap();
    assert_eq!(vertex_code.len(), 300);
    for (register, file) in [(0x2c48, "vertex-shader-prefix.bin"), (0x2c08, "pixel-shader-complete.bin")] {
        let path = root.join(file);
        if !path.is_file() { continue; }
        let bytes = std::fs::read(path).unwrap();
        let address = (registers[&register] as u64) << 8 | (registers[&(register + 1)] as u64) << 40;
        mem.map(address, bytes.len() as u64, true, false).unwrap();
        mem.write(address, &bytes);
    }
    let log = std::fs::read_to_string(root.join("runtime.log")).unwrap();
    for line in log.lines().filter(|line| line.starts_with("private_raster_resource=")) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let name = fields[0].strip_prefix("private_raster_resource=").unwrap();
        let address = u64::from_str_radix(fields[1].strip_prefix("address=0x").unwrap(), 16).unwrap();
        let bytes = std::fs::read(root.join(format!("raster-{name}.bin"))).unwrap();
        assert!(bytes.len() <= 65536);
        mem.map(address, bytes.len() as u64, true, false).unwrap();
        mem.write(address, &bytes);
    }
    let indices = std::fs::read(root.join("raster-draw-indices.bin")).unwrap();
    let indices = indices.chunks_exact(4).map(|word| u32::from_le_bytes(word.try_into().unwrap())).collect::<Vec<_>>();
    assert_eq!(indices, [0, 1, 2, 2, 1, 3]);
    RasterSnapshot { root, mem, registers, indices, vertex_code }
}
