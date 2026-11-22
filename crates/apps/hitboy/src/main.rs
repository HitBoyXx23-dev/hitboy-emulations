#![windows_subsystem = "windows"]

fn main() {
    if let Err(e) = frontend::run(&frontend::theme::HITBOY) {
        eprintln!("{e}");
    }
}
