use super::*;
use std::borrow::Cow;

const MODULE_BASE: u64 = 0x4000_0000;
const GLOBAL_BASE: u64 = 0x7100_0000;
const BOOT_BASE: u64 = 0x7200_0000;

pub(super) struct Module<'a> {
    pub bytes: Cow<'a, [u8]>,
    pub elf: Elf,
    pub base: u64,
    pub bindings: Vec<Option<u64>>,
}

pub(super) fn map_modules<'a>(mem: &mut Mem, files: &'a [Vec<u8>]) -> Result<Vec<Module<'a>>, String> {
    if files.len() > 16 { return Err("Too many startup modules.".into()); }
    let mut modules = Vec::new();
    let mut next = MODULE_BASE;
    for file in files {
        let bytes = crate::image::program_image(file)?;
        let elf = Elf::parse(&bytes)?;
        let minimum = elf.segments.iter().filter(|segment| segment.is_loadable()).map(|segment| segment.vaddr).min().ok_or("A module has no loadable segments.")?;
        let base = next.checked_sub(minimum).ok_or("The module cannot be placed in the module address region.")?;
        for segment in elf.segments.iter().filter(|segment| segment.is_loadable()) {
            let address = base.checked_add(segment.vaddr).ok_or("Module address overflow.")?;
            let end = address.checked_add(segment.mem_size).ok_or("Module size overflow.")?;
            if end >= 0x6F00_0000 { return Err("The modules exceed their reserved address region.".into()); }
            mem.map(address, segment.mem_size, segment.flags & 2 != 0, segment.flags & 1 != 0)?;
            let range_end = segment.offset.checked_add(segment.file_size).ok_or("Module segment overflow.")?;
            let data = bytes.get(segment.offset as usize..range_end as usize).ok_or("A module segment is truncated.")?;
            mem.write(address, data);
            next = next.max((end + 0xFFFF) & !0xFFFF);
        }
        modules.push(Module { bytes, elf, base, bindings: Vec::new() });
    }
    Ok(modules)
}

pub(super) fn exports(elf: &Elf, base: u64, modules: &[Module<'_>]) -> HashMap<String, u64> {
    let mut exports = HashMap::new();
    for (elf, base) in std::iter::once((elf, base)).chain(modules.iter().map(|module| (&module.elf, module.base))) {
        for symbol in elf.symbols.iter().filter(|symbol| symbol.defined) {
            let name = symbol.name.split('#').next().unwrap_or("");
            if !name.is_empty() { exports.entry(name.to_string()).or_insert(base + symbol.value); }
        }
    }
    exports
}

pub(super) fn bind(mem: &mut Mem, elf: &Elf, exports: &HashMap<String, u64>, imports: &mut Vec<String>, globals: &mut HashMap<String, u64>) -> Result<Vec<Option<u64>>, String> {
    let mut bindings = vec![None; elf.symbols.len()];
    for (index, symbol) in elf.symbols.iter().enumerate().skip(1).filter(|(_, symbol)| !symbol.defined) {
        let nid = symbol.name.split('#').next().unwrap_or("");
        if let Some(&address) = exports.get(nid) { bindings[index] = Some(address); continue; }
        let name = crate::nid::lookup(nid).unwrap_or(nid);
        if symbol.kind == 1 || matches!(name, "__stack_chk_guard" | "__progname") {
            if let Some(&address) = globals.get(name) { bindings[index] = Some(address); continue; }
            if !matches!(name, "__stack_chk_guard" | "__progname") {
                return Err(format!("Imported runtime data {name} needs its defining module. Add the game's bundled runtime modules."));
            }
            let address = GLOBAL_BASE + globals.len() as u64 * 256;
            mem.map(address, 256, true, false)?;
            if name == "__stack_chk_guard" { mem.write_u64(address, 0x5A5A_5A5A_1234_5678); }
            if name == "__progname" {
                mem.write_u64(address, address + 8);
                mem.write(address + 8, b"eboot.bin\0");
            }
            globals.insert(name.to_string(), address);
            bindings[index] = Some(address);
        } else {
            let index_in_stubs = imports.len() as u64;
            imports.push(if name.is_empty() { format!("symbol {index}") } else { name.to_string() });
            bindings[index] = Some(STUB_BASE + index_in_stubs * 16);
        }
    }
    Ok(bindings)
}

pub(super) fn tls_symbols(elf: &Elf, modules: &[Module<'_>]) -> HashMap<String, (u64, u64)> {
    let mut symbols = HashMap::new();
    for (index, elf) in std::iter::once(elf).chain(modules.iter().map(|module| &module.elf)).enumerate() {
        for symbol in elf.symbols.iter().filter(|symbol| symbol.defined && symbol.kind == 6) {
            symbols.insert(symbol.name.split('#').next().unwrap_or("").to_string(), (index as u64 + 1, symbol.value));
        }
    }
    symbols
}

pub(super) fn relocate(mem: &mut Mem, elf: &Elf, base: u64, bindings: &[Option<u64>], module_id: u64, tls_size: u64, tls_offsets: &HashMap<u64, u64>, tls_symbols: &HashMap<String, (u64, u64)>) -> Result<(), String> {
    for relocation in &elf.relocs {
        let symbol = reloc_sym_addr(relocation.sym, base, elf, bindings);
        let value = match relocation.kind {
            8 => base.wrapping_add_signed(relocation.addend),
            1 => symbol.wrapping_add_signed(relocation.addend),
            6 | 7 => symbol,
            16..=18 => {
                let (module, value) = if relocation.sym == 0 { (module_id, 0) } else {
                    let symbol = elf.symbols.get(relocation.sym as usize).ok_or("The TLS relocation references a missing symbol.")?;
                    if symbol.defined || (symbol.kind == 3 && symbol.name.is_empty()) { (module_id, symbol.value) } else {
                        *tls_symbols.get(symbol.name.split('#').next().unwrap_or("")).ok_or("The imported TLS symbol has no defining module.")?
                    }
                };
                let offset = value.wrapping_add_signed(relocation.addend);
                match relocation.kind {
                    16 => module,
                    17 => offset,
                    _ => tls_offsets.get(&module).ok_or("The TLS relocation references a module without TLS.")?.wrapping_sub(tls_size).wrapping_add(offset),
                }
            }
            0 => continue,
            kind => return Err(format!("ELF relocation type {kind} is not implemented.")),
        };
        let target = base.checked_add(relocation.offset).ok_or("Relocation address overflow.")?;
        if !mem.write_u64(target, value) {
            mem.map(target, 8, true, false)?;
            mem.write_u64(target, value);
        }
    }
    Ok(())
}

pub(super) fn tls(elf: &Elf, file: &[u8], modules: &[Module<'_>]) -> Result<(Vec<u8>, u64, HashMap<u64, u64>), String> {
    let mut entries = Vec::new();
    for (index, (elf, bytes)) in std::iter::once((elf, file)).chain(modules.iter().map(|module| (&module.elf, module.bytes.as_ref()))).enumerate() {
        if let Some(tls) = elf.segments.iter().find(|segment| segment.kind == "tls") {
            if tls.file_size > tls.mem_size || tls.mem_size > 0xE000 { return Err("The module's TLS image exceeds the supported size.".into()); }
            let data = bytes.get(tls.offset as usize..(tls.offset + tls.file_size) as usize).ok_or("The module's TLS image is truncated.")?;
            entries.push((index as u64 + 1, data, (tls.mem_size + 15) & !15));
        }
    }
    let mut image = Vec::new();
    let mut offsets = HashMap::new();
    for (id, data, size) in entries.into_iter().rev() {
        offsets.insert(id, image.len() as u64);
        let start = image.len();
        if start + size as usize > 0xE000 { return Err("Combined module TLS exceeds the supported size.".into()); }
        image.resize(start + size as usize, 0);
        image[start..start + data.len()].copy_from_slice(data);
    }
    let size = image.len() as u64;
    Ok((image, size, offsets))
}

pub(super) fn bootstrap(mem: &mut Mem, modules: &[Module<'_>], entry: u64, args: u64, exit: u64) -> Result<u64, String> {
    let initializers: Vec<u64> = modules.iter().filter_map(|module| module.elf.init.map(|init| module.base + init)).collect();
    if initializers.is_empty() { return Ok(entry); }
    let mut code = vec![0x48, 0x83, 0xEC, 8];
    for address in initializers {
        code.extend_from_slice(&[0x31, 0xFF, 0x31, 0xF6, 0x31, 0xD2, 0x48, 0xB8]);
        code.extend_from_slice(&address.to_le_bytes());
        code.extend_from_slice(&[0xFF, 0xD0]);
    }
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 8, 0x48, 0xBF]);
    code.extend_from_slice(&args.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xBE]);
    code.extend_from_slice(&exit.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xB8]);
    code.extend_from_slice(&entry.to_le_bytes());
    code.extend_from_slice(&[0xFF, 0xE0]);
    mem.map(BOOT_BASE, code.len() as u64, false, true)?;
    mem.write(BOOT_BASE, &code);
    Ok(BOOT_BASE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::{Reloc, Sym};

    fn symbol(name: &str, kind: u8, defined: bool, value: u64) -> Sym {
        Sym { name: name.into(), value, kind, size: 8, defined }
    }

    #[test]
    fn imported_runtime_data_and_functions_bind_to_module_exports() {
        let runtime = Elf { symbols: vec![symbol("runtime-data#A", 1, true, 0x100), symbol("runtime-call#A", 2, true, 0x200)], ..Elf::default() };
        let modules = vec![Module { bytes: Cow::Borrowed(&[]), elf: runtime, base: MODULE_BASE, bindings: Vec::new() }];
        let main = Elf { symbols: vec![symbol("", 0, false, 0), symbol("runtime-data#B", 1, false, 0), symbol("runtime-call#B", 2, false, 0)], ..Elf::default() };
        let mut imports = Vec::new();
        let bindings = bind(&mut Mem::new().unwrap(), &main, &exports(&main, 0, &modules), &mut imports, &mut HashMap::new()).unwrap();
        assert_eq!(bindings[1], Some(MODULE_BASE + 0x100));
        assert_eq!(bindings[2], Some(MODULE_BASE + 0x200));
        assert!(imports.is_empty());
        assert!(bind(&mut Mem::new().unwrap(), &main, &HashMap::new(), &mut imports, &mut HashMap::new()).is_err());
    }

    #[test]
    fn cross_module_tls_and_unnamed_section_relocations_use_correct_module() {
        let elf = Elf {
            symbols: vec![symbol("", 0, false, 0), symbol("runtime-tls#A", 6, false, 0), symbol("", 3, false, 0)],
            relocs: vec![Reloc { offset: 0, kind: 16, sym: 1, addend: 0 }, Reloc { offset: 8, kind: 17, sym: 1, addend: 3 }, Reloc { offset: 16, kind: 18, sym: 1, addend: 3 }, Reloc { offset: 24, kind: 16, sym: 2, addend: 0 }],
            ..Elf::default()
        };
        let mut mem = Mem::new().unwrap();
        mem.map(0x50000000, 4096, true, false).unwrap();
        relocate(&mut mem, &elf, 0x50000000, &[None; 3], 1, 48, &HashMap::from([(1, 32), (2, 0)]), &HashMap::from([("runtime-tls".into(), (2, 8))])).unwrap();
        assert_eq!(mem.read_u64(0x50000000), 2);
        assert_eq!(mem.read_u64(0x50000008), 11);
        assert_eq!(mem.read_u64(0x50000010), (-37i64) as u64);
        assert_eq!(mem.read_u64(0x50000018), 1);
    }

    #[test]
    fn dependency_initializer_executes_before_the_program() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        machine.mem.map(MODULE_BASE, 4096, true, true).unwrap();
        let mut code = vec![0x48, 0xB8];
        code.extend_from_slice(&(MODULE_BASE + 0x100).to_le_bytes());
        code.extend_from_slice(&[0x48, 0xC7, 0, 42, 0, 0, 0, 0xC3]);
        machine.mem.write(MODULE_BASE, &code);
        let module = Module { bytes: Cow::Borrowed(&[]), elf: Elf { init: Some(0), ..Elf::default() }, base: MODULE_BASE, bindings: Vec::new() };
        machine.cpu.rip = bootstrap(&mut machine.mem, &[module], machine.entry, machine.cpu.r[7], machine.cpu.r[6]).unwrap();
        assert_eq!(machine.run(10000).unwrap(), Stop::Exited(7));
        assert_eq!(machine.mem.read_u64(MODULE_BASE + 0x100), 42);
    }
}
