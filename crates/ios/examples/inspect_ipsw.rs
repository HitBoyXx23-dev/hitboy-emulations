fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: inspect_ipsw <path/to/firmware.ipsw>");
        std::process::exit(2);
    };
    match ios::ipsw::read_ipsw_file(std::path::Path::new(&path)) {
        Ok(info) => println!("{}", info.summary()),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
