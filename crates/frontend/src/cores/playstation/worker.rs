use ps5::machine::{Machine, Stop};
use std::sync::{Arc, Mutex, mpsc};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;

pub(super) struct Frame {
    pub pixels: Vec<u32>,
    pub size: (usize, usize),
}

#[derive(Default)]
struct State {
    frame: Option<Frame>,
    audio: Vec<f32>,
    message: Option<String>,
}

enum Work {
    Run(Option<mpsc::Sender<()>>),
    Reset(Vec<u8>, mpsc::Sender<Result<(), String>>),
}

pub(super) struct Worker {
    input: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
    sender: mpsc::SyncSender<Work>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn new(mut machine: Machine) -> Self {
        let input = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(State::default()));
        let (sender, receiver) = mpsc::sync_channel(1);
        let (thread_input, thread_stop, thread_state) = (input.clone(), stop.clone(), state.clone());
        let thread = std::thread::spawn(move || {
            let mut flips = 0;
            let mut stopped = false;
            while let Ok(work) = receiver.recv() {
                if thread_stop.load(Ordering::Acquire) { break; }
                match work {
                    Work::Reset(image, reply) => {
                        let result = machine.reload(&image);
                        flips = 0;
                        stopped = result.is_err();
                        *thread_state.lock().unwrap() = State { frame: None, audio: Vec::new(), message: result.as_ref().err().cloned() };
                        let _ = reply.send(result);
                    }
                    Work::Run(reply) => {
                        if !stopped {
                            for _ in 0..2 {
                                if thread_stop.load(Ordering::Acquire) { break; }
                                machine.pad_buttons = thread_input.load(Ordering::Relaxed);
                                let generation = machine.title_generation();
                                let message = match machine.run(100_000) {
                                    Ok(Stop::Running) if machine.unimplemented.is_empty() => None,
                                    Ok(Stop::Running) => Some("This program needs unsupported PlayStation system functions.".to_string()),
                                    Ok(Stop::Exited(code)) => Some(format!("PlayStation program exited with code {code}.")),
                                    Err(error) => Some(error),
                                };
                                if generation != machine.title_generation() {
                                    flips = 0;
                                    thread_state.lock().unwrap().frame = Some(Frame { pixels: vec![0; 640 * 360], size: (640, 360) });
                                }
                                if machine.video.flips != flips {
                                    if let Some((width, height, rgba)) = machine.framebuffer() {
                                        let pixels = rgba.chunks_exact(4).map(|pixel| {
                                            (pixel[0] as u32) << 16 | (pixel[1] as u32) << 8 | pixel[2] as u32
                                        }).collect();
                                        thread_state.lock().unwrap().frame = Some(Frame { pixels, size: (width as usize, height as usize) });
                                        flips = machine.video.flips;
                                    }
                                }
                                let mut pcm = Vec::new();
                                machine.take_audio(&mut pcm);
                                if !pcm.is_empty() {
                                    let mut guard = thread_state.lock().unwrap();
                                    if guard.audio.len() < 48_000 * 2 {
                                        guard.audio.extend(pcm);
                                    }
                                }
                                if let Some(mut message) = message {
                                    if let Some(title) = machine.active_title() { message = format!("{title}: {message}"); }
                                    if !machine.unimplemented.is_empty() {
                                        message.push_str(&format!(" Missing system functions: {}.", machine.unimplemented.join(", ")));
                                    }
                                    thread_state.lock().unwrap().message = Some(message);
                                    stopped = true;
                                    break;
                                }
                            }
                        }
                        if let Some(reply) = reply { let _ = reply.send(()); }
                    }
                }
            }
        });
        Self { input, stop, state, sender, thread: Some(thread) }
    }

    pub fn run(&self) -> Result<(), String> {
        match self.sender.try_send(Work::Run(None)) {
            Ok(()) | Err(mpsc::TrySendError::Full(_)) => Ok(()),
            Err(_) => Err("The PS4 execution worker stopped unexpectedly.".into()),
        }
    }

    pub fn run_blocking(&self) -> Result<(), String> {
        let (sender, receiver) = mpsc::channel();
        self.sender.send(Work::Run(Some(sender))).map_err(|_| "The PS4 execution worker stopped unexpectedly.".to_string())?;
        receiver.recv_timeout(std::time::Duration::from_secs(30)).map_err(|_| "The PS4 execution worker did not finish its instruction batch.".to_string())
    }

    pub fn set_buttons(&self, buttons: u32) {
        self.input.store(buttons, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn buttons(&self) -> u32 {
        self.input.load(Ordering::Relaxed)
    }

    pub fn take_audio(&self, out: &mut Vec<f32>) {
        if let Ok(mut state) = self.state.lock() {
            out.append(&mut state.audio);
        }
    }

    pub fn update(&self) -> (Option<Frame>, Option<String>) {
        match self.state.lock() {
            Ok(mut state) => (state.frame.take(), state.message.clone()),
            Err(_) => (None, Some("The PS4 execution worker stopped unexpectedly.".into())),
        }
    }

    pub fn reset(&self, image: &[u8]) -> Result<(), String> {
        let (sender, receiver) = mpsc::channel();
        self.input.store(0, Ordering::Relaxed);
        self.sender.send(Work::Reset(image.to_vec(), sender)).map_err(|_| "The PS4 execution worker stopped unexpectedly.".to_string())?;
        receiver.recv_timeout(std::time::Duration::from_secs(30)).map_err(|_| "The PS4 execution worker did not finish resetting.".to_string())?
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.sender.try_send(Work::Run(None));
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}
