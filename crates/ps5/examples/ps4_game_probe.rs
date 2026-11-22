use ps5::machine::{Machine, Stop};
use std::{path::PathBuf, time::Instant};

fn main() -> Result<(), String> {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if !(2..=4).contains(&arguments.len()) { return Err("Supply a game folder and a private report directory".into()); }
    if arguments.get(2).is_some_and(|value| value != "trace" && value != "continue" && value != "dialogs") { return Err("The optional diagnostic mode must be trace, continue or dialogs".into()); }
    let trace = arguments.get(2).is_some_and(|value| value == "trace");
    let dialogs = arguments.get(2).is_some_and(|value| value == "dialogs");
    let continue_missing = trace || dialogs || arguments.get(2).is_some_and(|value| value == "continue");
    let duration = match std::env::var("HITBOY_PS4_PROBE_SECONDS") {
        Ok(value) => value.parse::<u64>().ok().filter(|seconds| (1..=600).contains(seconds))
            .ok_or("HITBOY_PS4_PROBE_SECONDS must be between 1 and 600")?,
        Err(std::env::VarError::NotPresent) => 60,
        Err(_) => return Err("HITBOY_PS4_PROBE_SECONDS must contain a number".into()),
    };
    let stop_on_graphics_submission = std::env::var("HITBOY_PS4_STOP_ON_GRAPHICS_SUBMISSION").is_ok_and(|value| value == "1");
    let stop_on_memory_error = std::env::var("HITBOY_PS4_STOP_ON_MEMORY_ERROR").is_ok_and(|value| value == "1");
    let game = PathBuf::from(&arguments[0]);
    let output = PathBuf::from(&arguments[1]);
    std::fs::create_dir_all(&output).map_err(|error| error.to_string())?;
    let mut machine = Machine::load_game(&game)?;
    if let Some(option) = arguments.get(3) {
        if option != "--mount-paks" { return Err("The fourth argument must be --mount-paks".into()); }
        let entries = machine.mount_read_only_paks(&game.join("fortnitegame/content/paks"))?;
        println!("Mounted {entries} read-only archive entries; index checksums verified");
    }
    let started = Instant::now();
    let mut next_report = 0;
    let mut last_calls = 0;
    let mut steps = 0u64;
    let mut reports = vec![format!("diagnostic_only=true continue_unimplemented={continue_missing} duration_seconds={duration}")];
    let mut outcome = "Diagnostic time limit reached".to_string();
    while started.elapsed().as_secs() < duration {
        let batch = if trace || dialogs { 1 } else { 100_000 };
        let result = machine.run(batch);
        steps += batch;
        if (trace || dialogs) && machine.calls != last_calls {
            last_calls = machine.calls;
            if trace {
                let line = format!("call={} {:?}", machine.calls, machine.last_call);
                println!("{line}"); reports.push(line);
            }
            if dialogs && machine.last_call.as_ref().is_some_and(|(name, _)| name == "sceMsgDialogOpen") {
                if let Some(message) = dialog_message(&machine) {
                    let line = format!("game_dialog={message:?}");
                    println!("{line}"); reports.push(line);
                }
            }
        }
        if started.elapsed().as_secs() >= next_report {
            let report = format!("elapsed={} steps={} rip={:#x} calls={} last={:?} flips={} missing={:?}", started.elapsed().as_secs(), steps, machine.cpu.rip, machine.calls, machine.last_call, machine.video.flips, machine.unimplemented);
            println!("{report}");
            reports.push(report);
            if next_report % 30 == 0 {
                reports.push(format!("sample_returns={:x?}", frame_pointer_returns(&machine)));
                reports.push(machine.file_service_report());
                reports.push(machine.memory_service_report());
                reports.push(machine.graphics_service_report());
                std::fs::write(output.join("progress.log"), reports.join("\n")).map_err(|error| error.to_string())?;
            }
            next_report += 5;
        }
        if !matches!(result, Ok(Stop::Running)) || (!continue_missing && !machine.unimplemented.is_empty()) {
            outcome = format!("Execution stopped: {result:?}");
            break;
        }
        if stop_on_memory_error && machine.output.contains("Memory batch failed:") {
            outcome = "Stopped at the first failed memory batch for diagnosis".into();
            break;
        }
        if stop_on_graphics_submission && machine.graphics_submission_count() > 0 {
            outcome = "Stopped at graphics submission for diagnosis; rendering is not implemented".into();
            break;
        }
        if machine.video.flips > 0 {
            outcome = "The game submitted a display frame; gameplay is not yet verified".into();
            break;
        }
    }
    let mut code = [0u8; 64];
    machine.mem.read(machine.cpu.rip.saturating_sub(16), &mut code);
    reports.push(format!("registers={:x?} fs={:x} code_start={:#x} code={:02x?}", machine.cpu.r, machine.cpu.base[4], machine.cpu.rip.saturating_sub(16), code));
    reports.push(format!("frame_pointer_returns={:x?}", frame_pointer_returns(&machine)));
    if machine.video.flips > 0 {
        if let Some((width, height, rgba)) = machine.framebuffer() {
            let mut picture = format!("P6\n{width} {height}\n255\n").into_bytes();
            picture.extend(rgba.chunks_exact(4).flat_map(|pixel| [pixel[0], pixel[1], pixel[2]]));
            std::fs::write(output.join("first-frame.ppm"), picture).map_err(|error| error.to_string())?;
        }
    }
    for module in [game.join("eboot.bin"), game.join("sce_module/libc.prx")] {
        if let Ok(info) = ps5::read_game(&module) {
            if let Some(elf) = info.elf {
                for import in elf.imports {
                    if machine.unimplemented.contains(&import.nid) {
                        let detail = format!("unresolved={} library={}", import.nid, import.library);
                        println!("{detail}");
                        reports.push(detail);
                    }
                }
            }
        }
    }
    let summary = format!("{outcome}\nsteps={steps}\nelapsed_seconds={:.2}\nframes={}\nlast_call={:?}\nmissing={:?}\nguest_output_bytes={}\n", started.elapsed().as_secs_f64(), machine.video.flips, machine.last_call, machine.unimplemented, machine.output.len());
    let messages = machine.output.lines().filter(|line| {
        let lower = line.to_ascii_lowercase();
        line.len() <= 512 && ["failed", "error", "missing", "could not", "fatal", "assert", "out of memory"].iter().any(|word| lower.contains(word))
            && !["token", "password", "authorization", "secret", "credential"].iter().any(|word| lower.contains(word))
    }).collect::<Vec<_>>();
    let selected = if messages.len() > 64 {
        messages[..16].iter().chain(messages[messages.len() - 48..].iter()).copied().collect::<Vec<_>>()
    } else { messages };
    for line in selected {
        let message = format!("startup_message={line:?}");
        println!("{message}");
        reports.push(message);
    }
    if std::env::var("HITBOY_PS4_CAPTURE_GRAPHICS_COMMANDS").is_ok_and(|value| value == "1") {
        for (index, (_, bytes)) in machine.first_graphics_command_buffers().iter().enumerate() {
            std::fs::write(output.join(format!("first-commands-{index:02}.pm4")), bytes).map_err(|error| error.to_string())?;
        }
        if let Some(bytes) = machine.pending_graphics_command_buffer() {
            std::fs::write(output.join("pending-commands.pm4"), bytes).map_err(|error| error.to_string())?;
        }
        let registers = machine.graphics_register_snapshot().into_iter().flat_map(|(register, value)| register.to_le_bytes().into_iter().chain(value.to_le_bytes())).collect::<Vec<_>>();
        std::fs::write(output.join("graphics-registers.bin"), registers).map_err(|error| error.to_string())?;
        for (name, address, bytes) in machine.raster_resource_snapshots() {
            std::fs::write(output.join(format!("raster-{name}.bin")), &bytes).map_err(|error| error.to_string())?;
            reports.push(format!("private_raster_resource={name} address={address:#x} bytes={}", bytes.len()));
        }
        for (stage, address, bytes) in machine.raster_shader_snapshots() {
            std::fs::write(output.join(format!("{stage}-shader-prefix.bin")), &bytes).map_err(|error| error.to_string())?;
            reports.push(format!("private_{stage}_shader_prefix_address={address:#x} bytes={}", bytes.len()));
        }
        if let Some((address, bytes)) = machine.compute_shader_prefix() {
            std::fs::write(output.join("compute-shader-prefix.bin"), &bytes).map_err(|error| error.to_string())?;
            let registers = machine.compute_register_snapshot().into_iter().flat_map(|(register, value)| register.to_le_bytes().into_iter().chain(value.to_le_bytes())).collect::<Vec<_>>();
            std::fs::write(output.join("compute-registers.bin"), registers).map_err(|error| error.to_string())?;
            reports.push(format!("private_compute_shader_prefix_address={address:#x} bytes={}", bytes.len()));
            for (slot, buffer_address, buffer) in machine.compute_buffer_snapshots() {
                std::fs::write(output.join(format!("compute-buffer-{slot:02}.bin")), &buffer).map_err(|error| error.to_string())?;
                reports.push(format!("private_compute_buffer_slot={slot} address={buffer_address:#x} bytes={}", buffer.len()));
            }
        }
    }
    let threads = machine.thread_report();
    println!("{threads}"); reports.push(threads);
    let services = machine.file_service_report();
    println!("{services}");
    reports.push(services);
    let graphics = machine.graphics_service_report();
    println!("{graphics}"); reports.push(graphics);
    let memory = machine.memory_service_report();
    println!("{memory}"); reports.push(memory);
    println!("{summary}");
    reports.push(summary);
    std::fs::write(output.join("runtime.log"), reports.join("\n")).map_err(|error| error.to_string())?;
    Ok(())
}
fn dialog_message(machine: &Machine) -> Option<String> {
    let (_, arguments) = machine.last_call.as_ref()?;
    let mut parameter = [0; 80];
    if !machine.mem.read(arguments[0], &mut parameter) { return None; }
    let size = u64::from_le_bytes(parameter[48..56].try_into().ok()?);
    let mode = u32::from_le_bytes(parameter[56..60].try_into().ok()?);
    if size < 80 || size > 256 || !matches!(mode, 1 | 2) { return None; }
    let address = u64::from_le_bytes(parameter[if mode == 1 { 64..72 } else { 72..80 }].try_into().ok()?);
    let mut user_message = [0; 16];
    if !machine.mem.read(address, &mut user_message) { return None; }
    let text = u64::from_le_bytes(user_message[8..16].try_into().ok()?);
    let message = machine.mem.read_cstr(text);
    let lower = message.to_ascii_lowercase();
    if message.len() > 1024 || ["token", "password", "authorization", "secret", "credential"].iter().any(|word| lower.contains(word)) { return None; }
    Some(message)
}

fn frame_pointer_returns(machine: &Machine) -> Vec<u64> {
    let stack = machine.cpu.r[4];
    let Some(limit) = stack.checked_add(64 << 20) else { return Vec::new(); };
    let mut frame = machine.cpu.r[5];
    let mut returns = Vec::new();
    for _ in 0..24 {
        if frame < stack || frame > limit.saturating_sub(16) || frame & 7 != 0 { break; }
        let mut record = [0; 16];
        if !machine.mem.read(frame, &mut record) { break; }
        let previous = u64::from_le_bytes(record[..8].try_into().unwrap());
        let address = u64::from_le_bytes(record[8..].try_into().unwrap());
        if address == 0 || !machine.mem.read(address, &mut [0]) { break; }
        returns.push(address);
        if previous <= frame { break; }
        frame = previous;
    }
    returns
}
