use ps5::machine::{Machine, Stop};
use std::{path::PathBuf, time::Instant};

fn main() {
    let path = PathBuf::from(std::env::args_os().nth(1).expect("Supply the native remake ELF"));
    let image = std::fs::read(&path).unwrap();
    let mut machine = Machine::load_game_image(&image, path.parent().unwrap()).unwrap();
    let started = Instant::now();
    let mut next_report = 0;
    let mut steps = 0u64;
    while started.elapsed().as_secs() < 25 {
        let result = machine.run(100_000);
        steps += 100_000;
        if started.elapsed().as_secs() >= next_report {
            println!("elapsed={} steps={} rip={:#x} base={:#x} calls={} last={:?} flips={} missing={:?}", started.elapsed().as_secs(), steps, machine.cpu.rip, machine.base, machine.calls, machine.last_call, machine.video.flips, machine.unimplemented);
            next_report += 5;
        }
        match result {
            Ok(Stop::Running) if machine.unimplemented.is_empty() => (),
            other => { println!("stopped={other:?}"); break; }
        }
        if machine.video.flips > 0 { println!("A frame was submitted"); break; }
    }
    println!("output={}", machine.output.chars().take(4000).collect::<String>());
}