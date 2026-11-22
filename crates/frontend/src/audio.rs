//! Speaker output via cpal (WASAPI on Windows). Cores push interleaved stereo f32
//! samples at the device rate; the device callback drains them.
//!
//! Quality measures: the queue is kept near a small target latency by resampling the
//! input by up to ±0.5% (inaudible) instead of dropping or repeating chunks, which is
//! what causes crackles; volume changes ramp smoothly; a DC blocker removes the offset
//! some sound chips produce; underruns fade out instead of cutting to silence.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Queue level we steer towards, and the hard ceiling before audio is dropped.
const TARGET_LATENCY_S: f32 = 0.09;
const MAX_LATENCY_S: f32 = 0.25;
/// Output frames used to blend back in after the queue ran dry or skipped (about 5 ms).
const XFADE_FRAMES: u32 = 240;

pub struct Audio {
    _stream: cpal::Stream,
    queue: Arc<Mutex<VecDeque<f32>>>,
    gain: Arc<AtomicU32>,
    pub sample_rate: u32,
    pub volume: f32,
    /// Resampler state: fractional read position and the previous input frame.
    frac: f64,
    prev: (f32, f32),
}

struct Output {
    cur_gain: f32,
    last: (f32, f32),
    /// Frames of cross-fade left after an underrun (from `last` into fresh samples).
    xfade: u32,
    starved: bool,
    dc_in: (f32, f32),
    dc_out: (f32, f32),
}

impl Audio {
    pub fn new() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("No audio output device found.")?;
        let config = device.default_output_config().map_err(|e| e.to_string())?;
        let sample_rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::with_capacity(sample_rate as usize)));
        let gain = Arc::new(AtomicU32::new(0.8f32.to_bits()));
        let err = |e| eprintln!("audio stream error: {e}");
        let stream_cfg: cpal::StreamConfig = config.clone().into();
        let mk = || (queue.clone(), gain.clone(), Output { cur_gain: 0.0, last: (0.0, 0.0), xfade: 0, starved: false, dc_in: (0.0, 0.0), dc_out: (0.0, 0.0) });
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => {
                let (q, g, mut o) = mk();
                device.build_output_stream(&stream_cfg, move |data: &mut [f32], _| fill(data, channels, &q, &g, &mut o, |s| s), err, None)
            }
            cpal::SampleFormat::I16 => {
                let (q, g, mut o) = mk();
                device.build_output_stream(&stream_cfg, move |data: &mut [i16], _| fill(data, channels, &q, &g, &mut o, |s| (s * i16::MAX as f32) as i16), err, None)
            }
            cpal::SampleFormat::U16 => {
                let (q, g, mut o) = mk();
                device.build_output_stream(&stream_cfg, move |data: &mut [u16], _| fill(data, channels, &q, &g, &mut o, |s| ((s * 0.5 + 0.5) * u16::MAX as f32) as u16), err, None)
            }
            f => return Err(format!("Unsupported audio sample format {f:?}")),
        }
        .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(Self { _stream: stream, queue, gain, sample_rate, volume: 0.8, frac: 0.0, prev: (0.0, 0.0) })
    }

    /// Queues interleaved stereo samples, gently resampling to hold the target latency.
    pub fn push(&mut self, stereo: &[f32]) {
        self.gain.store(self.volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        let mut q = self.queue.lock().unwrap();
        let rate = self.sample_rate as f32;
        let fill = (q.len() / 2) as f32;
        let target = rate * TARGET_LATENCY_S;
        // Below target: stretch (more output frames); above: squeeze. Gentle near the target
        // (inaudible), stronger when a slow core lets the queue run low, so it never runs dry.
        let err = ((target - fill) / target).clamp(-1.0, 1.0) as f64;
        let strength = if err > 0.5 { 0.04 } else { 0.01 };
        let ratio = 1.0 + err * strength;
        let step = 1.0 / ratio;
        for f in stereo.chunks_exact(2) {
            let cur = (f[0], f[1]);
            while self.frac < 1.0 {
                let t = self.frac as f32;
                q.push_back(self.prev.0 + (cur.0 - self.prev.0) * t);
                q.push_back(self.prev.1 + (cur.1 - self.prev.1) * t);
                self.frac += step;
            }
            self.frac -= 1.0;
            self.prev = cur;
        }
        let max = (rate * MAX_LATENCY_S) as usize * 2;
        if q.len() > max {
            // Emulation ran far ahead (e.g. after a stall): skip back to the target level,
            // blending across the jump so it doesn't click.
            let excess = (q.len() - (target as usize * 2)) & !1;
            let (a0, a1) = (q[0], q[1]);
            q.drain(..excess);
            let n = (XFADE_FRAMES as usize).min(q.len() / 2);
            for i in 0..n {
                let t = i as f32 / n as f32;
                q[i * 2] = a0 + (q[i * 2] - a0) * t;
                q[i * 2 + 1] = a1 + (q[i * 2 + 1] - a1) * t;
            }
        }
    }

    pub fn clear(&self) {
        self.queue.lock().unwrap().clear();
    }
}

fn fill<T: Copy + Default>(data: &mut [T], channels: usize, q: &Mutex<VecDeque<f32>>, gain: &AtomicU32, o: &mut Output, conv: impl Fn(f32) -> T) {
    let target_gain = f32::from_bits(gain.load(Ordering::Relaxed));
    let mut q = q.lock().unwrap();
    for frame in data.chunks_mut(channels) {
        let (l, r) = if q.len() >= 2 {
            let fresh = (q.pop_front().unwrap(), q.pop_front().unwrap());
            if o.starved {
                o.starved = false;
                o.xfade = XFADE_FRAMES;
            }
            if o.xfade > 0 {
                // Blend from the held value into the new audio instead of jumping to it.
                let t = 1.0 - o.xfade as f32 / XFADE_FRAMES as f32;
                o.xfade -= 1;
                (o.last.0 + (fresh.0 - o.last.0) * t, o.last.1 + (fresh.1 - o.last.1) * t)
            } else {
                fresh
            }
        } else {
            // Underrun: hold and slowly relax the last value rather than jumping to zero.
            o.starved = true;
            (o.last.0 * 0.9995, o.last.1 * 0.9995)
        };
        o.last = (l, r);
        // DC blocker (one-pole high-pass at ~20 Hz).
        let yl = l - o.dc_in.0 + 0.9975 * o.dc_out.0;
        let yr = r - o.dc_in.1 + 0.9975 * o.dc_out.1;
        o.dc_in = (l, r);
        o.dc_out = (yl, yr);
        // Volume ramps over ~20 ms so dragging the slider or muting never pops.
        o.cur_gain += (target_gain - o.cur_gain) * 0.002;
        let soft = |x: f32| {
            let x = x * o.cur_gain;
            if x.abs() <= 0.9 {
                x
            } else {
                x.signum() * (0.9 + 0.1 * ((x.abs() - 0.9) * 10.0).tanh())
            }
        };
        let (l, r) = (soft(yl), soft(yr));
        for (i, out) in frame.iter_mut().enumerate() {
            *out = conv(match i {
                0 => l,
                1 => r,
                _ => (l + r) * 0.5,
            });
        }
    }
}
