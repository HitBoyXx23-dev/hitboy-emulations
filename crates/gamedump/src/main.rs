//! Game Dump Helper: `gamedump <command>`.

use std::path::Path;
use std::time::Instant;

const USAGE: &str = "Game Dump Helper (HitBoy's Emulations)

  gamedump check <file or folder>   Say what a dump is and whether it looks complete
  gamedump hash <file>              SHA-1 and CRC-32 (compare with redump.org)
  gamedump drives                   List disc drives
  gamedump rip <drive> <out.iso>    Copy a plain data disc (2048-byte sectors) to an ISO
  gamedump run <eboot.bin|folder> [steps]   Try running a decrypted PlayStation 4/5 program on our own CPU
  gamedump ps-native <eboot.bin|folder>    Run a decrypted PlayStation program directly on Windows x64
  gamedump xbox <package folder>    Inspect an Xbox One / Series package, then run it natively
  gamedump xbox --inspect-only <package folder|manifest>   Check native imports without starting the title
  gamedump xbox-emu <package folder|manifest> [steps]   Probe the Xbox interpreter and stop on missing imports
  gamedump receive [folder] [--max-gb N]   Receive files from another device on your network (default limit 64 GB per file)
  gamedump switch <game.nsp|xci>    Try running a Switch game on our own ARM64 core (needs your prod.keys)
  gamedump ps4boot <kernel.elf>     Bring-up: start a PS4 kernel from your own console and list the hardware it needs
  gamedump guide                    How to get games from your own consoles and discs

Only use this with games you own, from discs and consoles you own.";

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let code = match a.first().map(String::as_str) {
        Some("check") if a.len() >= 2 => check(&a[1..]),
        Some("hash") if a.len() >= 2 => hash(&a[1]),
        Some("drives") => drives(),
        Some("rip") if a.len() >= 3 => rip(&a[1], &a[2]),
        Some("run") if a.len() >= 2 => run(&a[1], a.get(2).map(String::as_str)),
        Some("ps-native") if a.len() == 2 => ps_native(&a[1]),
        Some("xbox") if a.get(1).is_some_and(|arg| arg == "--inspect-only") && a.len() == 3 => xbox_native_imports(&a[2]),
        Some("xbox") if a.len() >= 2 && a[1] != "--inspect-only" => xbox(&a[1..]),
        Some("xbox-emu") if (2..=3).contains(&a.len()) => xbox_emu(&a[1], a.get(2)),
        Some("receive") => receive(&a[1..]),
        Some("switch") if a.len() >= 2 => switch_run(&a[1..]),
        Some("ps4boot") if a.len() >= 2 => ps4boot(&a[1..]),
        Some("guide") => {
            println!("{}", gamedump::GUIDE);
            0
        }
        _ => {
            println!("{USAGE}");
            if a.is_empty() { 0 } else { 2 }
        }
    };
    std::process::exit(code);
}

fn xbox_native_imports(path: &str) -> i32 {
    let (game, executable) = match xboxone::load_for_native(Path::new(path)) {
        Ok(title) => title,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let report = match xboxone::native::check_imports(&executable) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    println!("{} ({})", game.name, game.executable);
    println!("Resolved {} static imports on this PC. No guest code ran.", report.resolved);
    if !report.shimmed.is_empty() {
        println!("Xbox shims: {}", report.shimmed.join(", "));
    }
    if !report.missing_modules.is_empty() {
        println!("Missing libraries: {}", report.missing_modules.join(", "));
    }
    if !report.unimplemented.is_empty() {
        println!("Unresolved imports: {}", report.unimplemented.join(", "));
    }
    i32::from(!report.is_complete())
}

fn xbox_emu(path: &str, steps: Option<&String>) -> i32 {
    let limit = if let Some(steps) = steps {
        match steps.parse::<u64>() {
            Ok(limit @ 1..=100_000_000) => limit,
            _ => {
                eprintln!("Set an instruction budget from 1 to 100000000.");
                return 2;
            }
        }
    } else {
        1_000_000
    };
    let (game, image) = match xboxone::load_for_native(Path::new(path)) {
        Ok(title) => title,
        Err(error) => { eprintln!("{error}"); return 1; }
    };
    let mut core = match xboxone::x64_emu::XboxX64Emu::load_pe_with_executable_name(&image, &game.executable) {
        Ok(core) => core,
        Err(error) => { eprintln!("{error}"); return 1; }
    };
    core.set_strict_imports();
    println!("{} ({})", game.name, game.devices.join(", "));
    let result = core.run(limit);
    if !result.output.is_empty() {
        print!("{}", result.output);
    }
    println!("\nExecuted {} interpreter steps.", result.instructions);
    if !result.api_calls.is_empty() {
        println!("Imports reached: {}", result.api_calls.join(", "));
    }
    if let Some(reason) = result.stopped_reason {
        eprintln!("{reason}");
        return 1;
    }
    println!("Guest exited with code {}.", result.exit_code);
    0
}

fn ps_native(path: &str) -> i32 {
    let path = Path::new(path);
    let executable = if path.is_dir() { path.join("eboot.bin") } else { path.to_path_buf() };
    let result = std::fs::read(&executable)
        .map_err(|error| format!("Could not read {}: {error}", executable.display()))
        .and_then(|image| ps5::native::run(&image));
    let report = match result {
        Ok(report) => report,
        Err(error) => { eprintln!("{error}"); return 1; }
    };
    println!("Guest exited with code {}. VideoOut flips: {}.", report.exit_code, report.video_flips);
    if !report.output.is_empty() {
        print!("{}", report.output);
    }
    if !report.unimplemented.is_empty() {
        eprintln!("Unimplemented PlayStation imports: {}", report.unimplemented.join(", "));
        return 1;
    }
    0
}

fn check(paths: &[String]) -> i32 {
    let mut worst = 0;
    for p in paths {
        let r = gamedump::identify(Path::new(p));
        println!("{p}\n{}\n", r.text());
        worst = worst.max(match r.verdict {
            gamedump::Verdict::Good => 0,
            gamedump::Verdict::Warning => 0,
            gamedump::Verdict::Problem => 1,
        });
    }
    worst
}

fn hash(p: &str) -> i32 {
    let start = Instant::now();
    let mut last = 0u64;
    let r = gamedump::hash_file(Path::new(p), &mut |n| {
        if n - last >= 256 << 20 {
            eprintln!("  {} MB...", n >> 20);
            last = n;
        }
    });
    match r {
        Ok(h) => {
            println!("{p}\n  size  {} bytes\n  SHA-1 {}\n  CRC-32 {}\n  ({:.1}s)", h.bytes, h.sha1, h.crc32, start.elapsed().as_secs_f32());
            0
        }
        Err(e) => {
            eprintln!("Can't read {p}: {e}");
            1
        }
    }
}

fn drives() -> i32 {
    let d = gamedump::optical_drives();
    if d.is_empty() {
        println!("No disc drives found. (Drive listing works on Windows; on other systems use your disc tool's own list.)");
    } else {
        for x in d {
            println!("{x}");
        }
    }
    0
}

fn rip(drive: &str, out: &str) -> i32 {
    // A bare letter like "E:" is a raw volume; anything else is treated as a path.
    let src = if drive.len() <= 3 && drive.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && drive.contains(':') {
        format!("\\\\.\\{}", drive.trim_end_matches('\\'))
    } else {
        drive.to_string()
    };
    let start = Instant::now();
    let mut last = 0u64;
    match gamedump::copy_data_disc(&src, Path::new(out), &mut |done, total| {
        if done - last >= 8192 {
            if total > 0 {
                eprintln!("  {}%", done * 100 / total);
            }
            last = done;
        }
    }) {
        Ok(r) => {
            println!("Copied {} sectors ({} MB) in {:.0}s.", r.sectors, r.sectors * 2048 / 1_000_000, start.elapsed().as_secs_f32());
            if r.bad_sectors.is_empty() {
                println!("No unreadable sectors. Next: gamedump check \"{out}\" and gamedump hash \"{out}\".");
                0
            } else {
                println!("{} unreadable sector(s), zero-filled: first at {}. Clean the disc and try again, or try another drive.", r.bad_sectors.len(), r.bad_sectors[0]);
                1
            }
        }
        Err(e) => {
            eprintln!("Couldn't copy: {e}\nOn Windows, open the terminal as administrator to read a drive directly. Retail Xbox and Xbox 360 discs can't be copied this way.");
            1
        }
    }
}

/// Inspects an Xbox One / Series package and then runs its code on this PC's processor.
///
/// This only accepts packages that are already decrypted (Developer Mode apps, or content you
/// have legitimately taken off your own console). An encrypted retail package is reported as
/// such and refused: its keys live in the console and never come out.
fn xbox(paths: &[String]) -> i32 {
    let mut worst = 0;
    for p in paths {
        let path = Path::new(p);
        let report = xboxone::package::inspect(path);
        println!("{p}\n{}", report.text());

        match report.verdict {
            xboxone::package::Verdict::Encrypted => {
                println!("Nothing further to do: an encrypted package can't be read here.\n");
                worst = 1;
                continue;
            }
            xboxone::package::Verdict::NotAPackage | xboxone::package::Verdict::Unusable => {
                worst = 1;
                continue;
            }
            _ => {}
        }

        // Run it natively, off the main thread so a game that keeps running doesn't wedge the tool.
        let (tx, rx) = std::sync::mpsc::channel();
        let exe = std::fs::read(&report.executable).map_err(|e| e.to_string());
        match exe {
            Err(e) => {
                println!("Couldn't read the executable: {e}\n");
                worst = 1;
            }
            Ok(data) => {
                std::thread::spawn(move || {
                    let _ = tx.send(xboxone::native::run(&data));
                });
                match rx.recv_timeout(std::time::Duration::from_secs(20)) {
                    Ok(Ok(r)) => {
                        println!("Ran natively; exit code {}.", r.exit_code);
                        println!("Bound {} of the imported functions to this PC.", r.resolved);
                        if !r.shimmed.is_empty() {
                            println!("Our own code answered: {}.", r.shimmed.join(", "));
                        }
                        if !r.missing_modules.is_empty() {
                            println!("Libraries this PC can't provide: {}.", r.missing_modules.join(", "));
                        }
                        if !r.unimplemented.is_empty() {
                            println!("Functions not implemented yet ({}): {}", r.unimplemented.len(), r.unimplemented.join(", "));
                            worst = 1;
                        }
                        if !r.output.is_empty() {
                            println!("--- program output ---\n{}--- end ---", r.output);
                        }
                        println!();
                    }
                    Ok(Err(e)) => {
                        println!("Couldn't run it: {e}\n");
                        worst = 1;
                    }
                    Err(_) => println!("Still running after 20 seconds; left it running in the background.\n"),
                }
            }
        }
    }
    worst
}

/// Loads a decrypted PlayStation program and runs it on our x86-64 core, then reports what
/// it printed and which system functions it called that we don't provide yet.
fn run(p: &str, steps: Option<&str>) -> i32 {
    let max_steps = match steps {
        Some(value) => match value.parse::<u64>() {
            Ok(count) if (1..=5_000_000_000).contains(&count) => count,
            _ => {
                eprintln!("The step limit must be between 1 and 5000000000.");
                return 2;
            }
        },
        None => 5_000_000,
    };
    let path = Path::new(p);
    let file = if path.is_dir() { path.join("eboot.bin") } else { path.to_path_buf() };
    let mut m = match ps5::machine::Machine::load_game(&file) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Couldn't load it: {e}");
            return 1;
        }
    };
    if let Err(error) = m.set_game_root(file.parent().unwrap_or_else(|| Path::new("."))) {
        eprintln!("{error}");
        return 1;
    }
    println!("Loaded at base {:#x}, entry {:#x}. Running...", m.base, m.entry);
    let start = Instant::now();
    let mut steps = 0;
    let result = loop {
        let batch = (max_steps - steps).min(100_000);
        let result = m.run(batch);
        steps += batch;
        if !matches!(result, Ok(ps5::machine::Stop::Running)) || m.video.flips > 0 || steps >= max_steps || start.elapsed().as_secs() >= 120 {
            break result;
        }
    };
    if !m.output.is_empty() {
        println!("--- program output ---
{}--- end ---", m.output);
    }
    println!("{} system calls in {:.1}s.", m.calls, start.elapsed().as_secs_f32());
    if !m.unimplemented.is_empty() {
        println!("System functions it called that we don't provide yet ({}): {}", m.unimplemented.len(), m.unimplemented.join(", "));
    }
    if m.video.flips > 0 {
        println!("The game showed {} frame(s).", m.video.flips);
        if let Some((w, h, rgba)) = m.framebuffer() {
            let mut ppm = format!("P6
{w} {h}
255
").into_bytes();
            ppm.extend(rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
            match std::fs::write("frame.ppm", ppm) {
                Ok(()) => println!("Last frame ({w}x{h}) saved to frame.ppm."),
                Err(e) => eprintln!("Couldn't save frame.ppm: {e}"),
            }
        }
    }
    match result {
        Ok(ps5::machine::Stop::Exited(c)) => {
            println!("The program finished with exit code {c}.");
            0
        }
        Ok(ps5::machine::Stop::Running) => {
            println!("Stopped at {:#x} after reaching the diagnostic limit or first displayed frame.", m.cpu.rip);
            0
        }
        Err(e) => {
            println!("{e}");
            println!("CPU registers: {:x?}", m.cpu.r);
            if let Some((name, arguments)) = &m.last_call {
                println!("Last system call: {name} {:x?}", arguments);
            }
            1
        }
    }
}

fn receive(args: &[String]) -> i32 {
    let mut folder = None;
    let mut max_gb: Option<f64> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--max-gb" {
            match it.next().and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0) {
                Some(v) => max_gb = Some(v),
                None => {
                    eprintln!("--max-gb needs a number, for example: gamedump receive --max-gb 200");
                    return 2;
                }
            }
        } else {
            folder = Some(a.as_str());
        }
    }
    let dir = std::path::PathBuf::from(folder.unwrap_or("Received files"));
    let mut r = match gamedump::receive::Receiver::bind(&dir, 0) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Couldn't start the receiver: {e}");
            return 1;
        }
    };
    if let Some(g) = max_gb {
        r.max_file = (g * (1u64 << 30) as f64) as u64;
    }
    let host = gamedump::receive::lan_address().unwrap_or_else(|| "this-pc".into());
    println!("Receiving into: {}  (largest file allowed: {} GB)
", dir.display(), r.max_file >> 30);
    println!("On the other device (same Wi-Fi or cable network), open this address in its web browser:

    http://{host}:{}/{}/
", r.port(), r.token);
    println!("Anyone on your network who has that address can send files here, so don't share it.
It stops working when you close this window (Ctrl+C).");
    r.run(|p, n| println!("Received {} ({} MB)", p.display(), n >> 20));
    0
}

fn switch_run(args: &[String]) -> i32 {
    let mut game = None;
    let mut keys_path = None;
    let mut budget = 200_000_000u64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--keys" => keys_path = it.next().cloned(),
            "--steps" => budget = it.next().and_then(|v| v.parse().ok()).unwrap_or(budget),
            _ => game = Some(a.clone()),
        }
    }
    let Some(game) = game else {
        eprintln!("Usage: gamedump switch <game.nsp|game.xci> [--keys prod.keys] [--steps N]");
        return 2;
    };
    let keys_path = keys_path.unwrap_or_else(|| {
        let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
        let local = std::path::Path::new("prod.keys");
        if local.exists() { "prod.keys".into() } else { format!("{home}/.switch/prod.keys") }
    });
    let text = match std::fs::read_to_string(&keys_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Can't read the keys file {keys_path}: {e}\nDump prod.keys from your own Switch and pass it with --keys.");
            return 1;
        }
    };
    let keys = match switch::keys::Keys::parse(&text) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut p = match switch::launch::boot_game(Path::new(&game), &keys) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Can't start {game}: {e}");
            return 1;
        }
    };
    println!("Loaded program {:016x}; running up to {budget} instructions...", p.program_id);
    let start = Instant::now();
    let stop = p.run(budget);
    if !p.debug_output.is_empty() {
        println!("Program output:\n{}", p.debug_output);
    }
    println!("Stopped after {:.1}s at {:#x}: {stop:?}", start.elapsed().as_secs_f32(), p.cpu.pc);
    0
}

fn ps4boot(args: &[String]) -> i32 {
    let mut kernel = None;
    let (mut steps, mut ram_gb) = (50_000_000u64, 8u64);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--steps" => steps = it.next().and_then(|v| v.parse().ok()).unwrap_or(steps),
            "--ram-gb" => ram_gb = it.next().and_then(|v| v.parse().ok()).unwrap_or(ram_gb),
            _ => kernel = Some(a.clone()),
        }
    }
    let Some(kernel) = kernel else { return 2 };
    let data = match std::fs::read(&kernel) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Can't read {kernel}: {e}");
            return 1;
        }
    };
    if data.starts_with(&ps5::SELF_MAGIC_PS4) || data.starts_with(&ps5::SELF_MAGIC_PS5) {
        eprintln!("{}", ps5::ENCRYPTED_MESSAGE);
        return 1;
    }
    let mut sys = match ps5::system::System::new(&data, ram_gb) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let start = Instant::now();
    let outcome = sys.run(steps);
    print!("{}", sys.report(&outcome));
    println!("({:.1}s)", start.elapsed().as_secs_f32());
    0
}
