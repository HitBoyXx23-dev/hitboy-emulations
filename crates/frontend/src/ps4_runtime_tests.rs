use super::*;
use std::hash::{Hash, Hasher};

fn wait_for_picture(loaded: &mut cores::Loaded, path: &Path) -> u64 {
    let pointer = loaded.core.frame().as_ptr();
    let started = Instant::now();
    while started.elapsed().as_secs() < 150 {
        loaded.core.run_frame_blocking();
        assert!(loaded.core.runtime_message().is_none(), "{}", loaded.core.runtime_message().unwrap_or_default());
        if loaded.core.frame().as_ptr() != pointer {
            let (width, height) = loaded.core.size();
            assert_eq!((width, height), (1920, 1080));
            save_png(path, loaded.core.frame(), width, height).unwrap();
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            loaded.core.frame().hash(&mut hash);
            return hash.finish();
        }
    }
    panic!("The PS4 program did not display a new picture within the test limit");
}

#[test]
#[ignore]
fn ps4_launcher_opens_and_navigates() {
    let path = std::env::var_os("HITBOY_PS4_TEST_GAME").expect("Set HITBOY_PS4_TEST_GAME to the launcher ELF");
    let mut loaded = cores::load(Path::new(&path)).unwrap();
    let out = Path::new("target/ps4-qa/navigation");
    std::fs::create_dir_all(out).unwrap();
    let first = wait_for_picture(&mut loaded, &out.join("launcher-builds.png"));
    println!("Launcher first frame displayed");
    let mut pictures = std::collections::HashSet::from([first]);
    for (file, page) in [("browse", "Browse"), ("friends", "Play with Friends"), ("account", "Account"), ("settings", "Settings")] {
        loaded.core.set_buttons(0, 0);
        for _ in 0..8 { loaded.core.run_frame_blocking(); }
        loaded.core.set_buttons(0, 1 << 5);
        let picture = wait_for_picture(&mut loaded, &out.join(format!("launcher-{file}.png")));
        loaded.core.set_buttons(0, 0);
        assert!(pictures.insert(picture), "D-pad Down must reach a distinct {page} page");
        println!("Controller navigation displayed {page}");
    }
    for _ in 0..8 { loaded.core.run_frame_blocking(); }
    loaded.core.set_buttons(0, 1 << 1);
    let returned = wait_for_picture(&mut loaded, &out.join("launcher-returned-builds.png"));
    loaded.core.set_buttons(0, 0);
    assert_eq!(first, returned, "Circle must return to My builds");
    println!("Circle returned to My builds");
}

#[test]
#[ignore]
fn ps4_launcher_play_attempt() {
    let path = std::env::var_os("HITBOY_PS4_TEST_GAME").expect("Set HITBOY_PS4_TEST_GAME to the launcher ELF");
    let mut loaded = cores::load(Path::new(&path)).unwrap();
    let out = Path::new("target/ps4-qa/play-attempt");
    std::fs::create_dir_all(out).unwrap();
    wait_for_picture(&mut loaded, &out.join("before-play.png"));
    loaded.core.set_buttons(0, 0);
    for _ in 0..8 { loaded.core.run_frame_blocking(); }
    loaded.core.set_buttons(0, 1);
    let started = Instant::now();
    while started.elapsed().as_secs() < 150 {
        loaded.core.run_frame_blocking();
        if let Some(message) = loaded.core.runtime_message() {
            let message = message.to_string();
            let (width, height) = loaded.core.size();
            save_png(&out.join("after-play.png"), loaded.core.frame(), width, height).unwrap();
            std::fs::write(out.join("play-attempt.log"), &message).unwrap();
            panic!("Play did not reach gameplay: {message}");
        }
    }
    panic!("The Play attempt did not establish gameplay within the test limit");
}

#[test]
#[ignore]
fn ps4_launcher_captures_builds_and_browse() {
    let path = std::env::var_os("HITBOY_PS4_TEST_GAME").expect("Set HITBOY_PS4_TEST_GAME to the launcher ELF");
    let mut loaded = cores::load(Path::new(&path)).unwrap();
    let out = Path::new("target/ps4-qa/current-captures");
    std::fs::create_dir_all(out).unwrap();
    let builds = wait_for_picture(&mut loaded, &out.join("launcher-builds.png"));
    println!("Captured the current PS4 My builds screen.");
    loaded.core.set_buttons(0, 0);
    for _ in 0..8 { loaded.core.run_frame_blocking(); }
    loaded.core.set_buttons(0, 1 << 5);
    let browse = wait_for_picture(&mut loaded, &out.join("launcher-browse.png"));
    loaded.core.set_buttons(0, 0);
    assert_ne!(builds, browse);
    println!("Captured the current PS4 Browse screen. These are launcher images, not Fortnite gameplay.");
}

#[test]
#[ignore]
fn ps4_remake_launcher_opens_sandbox_and_returns() {
    let path = std::env::var_os("HITBOY_PS4_TEST_GAME").expect("Set HITBOY_PS4_TEST_GAME to the remake ELF");
    let mut loaded = cores::load(Path::new(&path)).unwrap();
    let output = Path::new("target/ps4-qa/remake-port");
    std::fs::create_dir_all(output).unwrap();
    let menu = wait_for_picture(&mut loaded, &output.join("native-remake-launcher.png"));
    loaded.core.set_buttons(0, 0);
    for _ in 0..8 { loaded.core.run_frame_blocking(); }
    loaded.core.set_buttons(0, 1);
    let sandbox = wait_for_picture(&mut loaded, &output.join("native-remake-sandbox.png"));
    assert_ne!(menu, sandbox, "Play must leave the launcher and render the native sandbox");
    loaded.core.set_buttons(0, 0);
    for _ in 0..8 { loaded.core.run_frame_blocking(); }
    loaded.core.set_buttons(0, 1 << 2);
    let started = Instant::now();
    let mut returned = false;
    while started.elapsed().as_secs() < 150 {
        loaded.core.run_frame_blocking();
        assert!(loaded.core.runtime_message().is_none(), "{}", loaded.core.runtime_message().unwrap_or_default());
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        loaded.core.frame().hash(&mut hash);
        if hash.finish() == menu {
            let (width, height) = loaded.core.size();
            save_png(&output.join("native-remake-returned-launcher.png"), loaded.core.frame(), width, height).unwrap();
            returned = true;
            break;
        }
    }
    loaded.core.set_buttons(0, 0);
    assert!(returned, "Touch pad must return to the same launcher");
    println!("Launcher -> Play -> native Rust sandbox -> launcher passed. Prototype visuals only; hardware and online play remain unverified.");
}
