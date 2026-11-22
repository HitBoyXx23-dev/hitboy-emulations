use std::path::PathBuf;

fn main() -> Result<(), String> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.len() != 2 { return Err("Supply a module and a private ELF output path".into()); }
    let source = std::fs::read(PathBuf::from(&args[0])).map_err(|error| error.to_string())?;
    let image = ps5::image::program_image(&source)?;
    let elf = ps5::elf::Elf::parse(&image)?;
    let wanted = ["sceFiosStatSync", "sceFiosFHStatSync", "sceFiosDirectoryExistsSync", "sceFiosFHGetOpenParams", "sceFiosFHGetPath", "sceFiosInitialize", "sceFiosFHOpenWithModeSync", "sceFiosFHOpenSync", "sceFiosFHReadSync", "sceFiosFHCloseSync", "sceFiosFHSeek", "sceFiosFHGetSize"];
    for symbol in elf.symbols.iter().filter(|symbol| symbol.defined) {
        let name = symbol.name.split('#').next().unwrap_or("");
        for wanted in wanted {
            if name == ps5::nid::nid(wanted) { println!("{wanted} address={:#x} bytes={}", symbol.value, symbol.size); }
        }
    }
    std::fs::write(PathBuf::from(&args[1]), &image).map_err(|error| error.to_string())?;
    Ok(())
}
