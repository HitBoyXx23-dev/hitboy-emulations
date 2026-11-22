//! Save states and rewind.
//!
//! A machine describes its state once, in a `sync` function that visits every field in a
//! fixed order. The same function saves (appends each value to a byte buffer) and loads
//! (reads each value back), so the two can never disagree. `#[derive(Stateful)]` writes that
//! function for you; `#[state(skip)]` leaves out fields that aren't state (ROM, file
//! handles, callbacks).
//!
//! `Rewind` keeps the last few seconds of snapshots compactly: the newest in full and each
//! older one as a small difference from the one after it, so stepping back is cheap.

// Lets `#[derive(Stateful)]`'s `::state::` paths resolve inside this crate too.
extern crate self as state;

pub use state_derive::Stateful;

use std::collections::VecDeque;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Save,
    Load,
}

/// A save or load in progress.
pub struct State {
    mode: Mode,
    buf: Vec<u8>,
    pos: usize,
    failed: bool,
}

impl State {
    pub fn saver() -> State {
        State { mode: Mode::Save, buf: Vec::new(), pos: 0, failed: false }
    }

    pub fn loader(data: &[u8]) -> State {
        State { mode: Mode::Load, buf: data.to_vec(), pos: 0, failed: false }
    }

    pub fn is_load(&self) -> bool {
        self.mode == Mode::Load
    }

    /// Marks the data as unusable (a value out of range, a size that doesn't match).
    pub fn fail(&mut self) {
        self.failed = true;
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Finished save: the bytes. Finished load: Ok only if everything was read and nothing
    /// was left over.
    pub fn finish(self) -> Result<Vec<u8>, String> {
        match self.mode {
            Mode::Save => Ok(self.buf),
            Mode::Load if self.failed => Err("The saved state doesn't match this game or version.".into()),
            Mode::Load if self.pos != self.buf.len() => Err("The saved state is from a different version of the emulator.".into()),
            Mode::Load => Ok(Vec::new()),
        }
    }

    /// Raw bytes in or out.
    pub fn raw(&mut self, b: &mut [u8]) {
        match self.mode {
            Mode::Save => self.buf.extend_from_slice(b),
            Mode::Load => {
                if self.failed || self.pos + b.len() > self.buf.len() {
                    self.failed = true;
                    return;
                }
                b.copy_from_slice(&self.buf[self.pos..self.pos + b.len()]);
                self.pos += b.len();
            }
        }
    }

    /// A tag that must match on load (guards against mixing up fields, versions, platforms).
    pub fn check(&mut self, tag: u32) {
        let mut v = tag;
        v.sync(self);
        if self.is_load() && v != tag {
            self.failed = true;
        }
    }

    /// Opaque blob of bytes with its length (an external C++ state block, for example).
    pub fn blob(&mut self, v: &mut Vec<u8>) {
        v.sync(self);
    }
}

/// Anything that can be saved and loaded through `State`.
pub trait Stateful {
    fn sync(&mut self, s: &mut State);

    /// Bulk version for slices; plain numbers override it with one block copy.
    fn sync_slice(v: &mut [Self], s: &mut State)
    where
        Self: Sized,
    {
        for x in v {
            x.sync(s);
        }
    }
}

macro_rules! ints {
    ($($t:ty),*) => {$(
        impl Stateful for $t {
            fn sync(&mut self, s: &mut State) {
                let mut b = self.to_le_bytes();
                s.raw(&mut b);
                if s.is_load() && !s.failed {
                    *self = <$t>::from_le_bytes(b);
                }
            }
            fn sync_slice(v: &mut [Self], s: &mut State) {
                const W: usize = std::mem::size_of::<$t>();
                if W == 1 {
                    // SAFETY: a slice of 1-byte integers is a slice of bytes.
                    let bytes = unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, v.len()) };
                    s.raw(bytes);
                    return;
                }
                let mut tmp = vec![0u8; v.len() * W];
                if !s.is_load() {
                    for (i, x) in v.iter().enumerate() {
                        tmp[i * W..(i + 1) * W].copy_from_slice(&x.to_le_bytes());
                    }
                }
                s.raw(&mut tmp);
                if s.is_load() && !s.failed {
                    for (i, x) in v.iter_mut().enumerate() {
                        *x = <$t>::from_le_bytes(tmp[i * W..(i + 1) * W].try_into().unwrap());
                    }
                }
            }
        }
    )*};
}
ints!(u8, i8, u16, i16, u32, i32, u64, i64, u128, i128, usize, isize);

impl Stateful for bool {
    fn sync(&mut self, s: &mut State) {
        let mut b = *self as u8;
        b.sync(s);
        *self = b != 0;
    }
}
impl Stateful for f32 {
    fn sync(&mut self, s: &mut State) {
        let mut b = self.to_bits();
        b.sync(s);
        *self = f32::from_bits(b);
    }
}
impl Stateful for f64 {
    fn sync(&mut self, s: &mut State) {
        let mut b = self.to_bits();
        b.sync(s);
        *self = f64::from_bits(b);
    }
}

impl<T: Stateful, const N: usize> Stateful for [T; N] {
    fn sync(&mut self, s: &mut State) {
        T::sync_slice(self, s);
    }
}

impl<T: Stateful + Default> Stateful for Vec<T> {
    fn sync(&mut self, s: &mut State) {
        let mut n = self.len() as u32;
        n.sync(s);
        if s.is_load() {
            if s.failed || n as usize > (1 << 28) {
                s.failed = true;
                return;
            }
            self.clear();
            self.resize_with(n as usize, T::default);
        }
        T::sync_slice(self, s);
    }
}

impl<T: Stateful + Default> Stateful for Option<T> {
    fn sync(&mut self, s: &mut State) {
        let mut some = self.is_some();
        some.sync(s);
        if s.is_load() {
            *self = if some { Some(T::default()) } else { None };
        }
        if let Some(v) = self {
            v.sync(s);
        }
    }
}

impl<T: Stateful + ?Sized> Stateful for Box<T> {
    fn sync(&mut self, s: &mut State) {
        (**self).sync(s);
    }
}

impl<A: Stateful, B: Stateful> Stateful for (A, B) {
    fn sync(&mut self, s: &mut State) {
        self.0.sync(s);
        self.1.sync(s);
    }
}
impl<A: Stateful, B: Stateful, C: Stateful> Stateful for (A, B, C) {
    fn sync(&mut self, s: &mut State) {
        self.0.sync(s);
        self.1.sync(s);
        self.2.sync(s);
    }
}

macro_rules! tuples {
    ($(($($n:ident $i:tt),+)),*) => {$(
        impl<$($n: Stateful),+> Stateful for ($($n,)+) {
            fn sync(&mut self, s: &mut State) {
                $( self.$i.sync(s); )+
            }
        }
    )*};
}
tuples!((A 0, B 1, C 2, D 3), (A 0, B 1, C 2, D 3, E 4), (A 0, B 1, C 2, D 3, E 4, F 5));

impl<T: Stateful + Default> Stateful for VecDeque<T> {
    fn sync(&mut self, s: &mut State) {
        let mut v: Vec<T> = if s.is_load() { Vec::new() } else { std::mem::take(self).into() };
        v.sync(s);
        *self = v.into();
    }
}

// ---------------------------------------------------------------- whole machines

const MAGIC: u32 = 0x4E53_5354; // "NSST"

/// A snapshot of a whole machine.
pub fn save<T: Stateful + ?Sized>(t: &mut T) -> Vec<u8> {
    let mut s = State::saver();
    s.check(MAGIC);
    t.sync(&mut s);
    s.finish().unwrap_or_default()
}

/// Restores a snapshot. The size is checked against the machine first, so a state from a
/// different game or version is refused before anything is changed.
pub fn load<T: Stateful + ?Sized>(t: &mut T, data: &[u8]) -> Result<(), String> {
    let expected = save(t).len();
    if expected != data.len() {
        return Err("This saved state doesn't match the game (or was made by a different version).".into());
    }
    let mut s = State::loader(data);
    s.check(MAGIC);
    t.sync(&mut s);
    s.finish().map(|_| ())
}

// ---------------------------------------------------------------- rewind

/// Compact difference between two snapshots: zero runs for unchanged bytes, literals for
/// changed ones. `prev` is the snapshot the difference is relative to.
pub fn delta(prev: &[u8], cur: &[u8]) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push(v as u8 | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }
    let mut out = Vec::new();
    varint(&mut out, cur.len() as u64);
    let x = |i: usize| cur[i] ^ prev.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while i < cur.len() {
        let start = i;
        if x(i) == 0 {
            while i < cur.len() && x(i) == 0 {
                i += 1;
            }
            varint(&mut out, ((i - start) as u64) << 1);
        } else {
            // A literal run ends at a stretch of 4+ unchanged bytes (cheaper to skip them).
            let mut zeros = 0;
            while i < cur.len() && zeros < 4 {
                zeros = if x(i) == 0 { zeros + 1 } else { 0 };
                i += 1;
            }
            let end = i - zeros;
            i = end;
            varint(&mut out, (((end - start) as u64) << 1) | 1);
            out.extend((start..end).map(x));
        }
    }
    out
}

/// Rebuilds the snapshot a `delta` was made from `prev` for.
pub fn apply(prev: &[u8], d: &[u8]) -> Option<Vec<u8>> {
    fn varint(d: &[u8], pos: &mut usize) -> Option<u64> {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let b = *d.get(*pos)?;
            *pos += 1;
            v |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
        }
    }
    let mut pos = 0;
    let len = varint(d, &mut pos)? as usize;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let t = varint(d, &mut pos)?;
        let n = (t >> 1) as usize;
        if t & 1 == 0 {
            let at = out.len();
            out.extend((at..at + n).map(|i| prev.get(i).copied().unwrap_or(0)));
        } else {
            let at = out.len();
            let lit = d.get(pos..pos + n)?;
            pos += n;
            out.extend(lit.iter().enumerate().map(|(k, b)| b ^ prev.get(at + k).copied().unwrap_or(0)));
        }
    }
    (out.len() == len).then_some(out)
}

/// The last few seconds of a game, as snapshots taken every few frames.
pub struct Rewind {
    /// Newest snapshot, in full.
    head: Option<Vec<u8>>,
    /// Older snapshots, oldest first; each is a delta to rebuild it from the one after it.
    older: VecDeque<Vec<u8>>,
    bytes: usize,
    pub max_snapshots: usize,
    pub max_bytes: usize,
}

impl Rewind {
    pub fn new(max_snapshots: usize, max_bytes: usize) -> Self {
        Rewind { head: None, older: VecDeque::new(), bytes: 0, max_snapshots, max_bytes }
    }

    pub fn len(&self) -> usize {
        self.older.len() + self.head.is_some() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    pub fn clear(&mut self) {
        self.head = None;
        self.older.clear();
        self.bytes = 0;
    }

    /// Records a new snapshot of the machine.
    pub fn push(&mut self, snap: Vec<u8>) {
        if let Some(h) = self.head.take() {
            // Store the old head as a difference from the new snapshot.
            let d = delta(&snap, &h);
            self.bytes += d.len();
            self.older.push_back(d);
        }
        self.head = Some(snap);
        while self.older.len() + 1 > self.max_snapshots || self.bytes > self.max_bytes {
            match self.older.pop_front() {
                Some(d) => self.bytes -= d.len(),
                None => break,
            }
        }
    }

    /// Steps back one snapshot: returns the one to restore (the newest if it's the last).
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        let head = self.head.take()?;
        match self.older.pop_back() {
            Some(d) => {
                self.bytes -= d.len();
                self.head = apply(&head, &d);
                if self.head.is_none() {
                    self.clear();
                    return Some(head);
                }
                Some(head)
            }
            None => {
                self.head = Some(head.clone());
                Some(head)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Stateful, Default, PartialEq, Debug)]
    struct Inner {
        a: u16,
        flag: bool,
    }

    #[derive(Stateful, Default, PartialEq, Debug, Clone, Copy)]
    enum Mode2 {
        #[default]
        Idle,
        Run,
        Done,
    }

    #[derive(Stateful, Default, PartialEq, Debug)]
    struct Machine {
        pc: u32,
        ram: Vec<u8>,
        regs: [u16; 4],
        pal: Vec<u32>,
        inner: Inner,
        mode: Mode2,
        maybe: Option<u64>,
        ratio: f32,
        #[state(skip)]
        rom: Vec<u8>,
    }

    fn sample() -> Machine {
        Machine { pc: 0x8000, ram: (0..300).map(|i| i as u8).collect(), regs: [1, 2, 3, 0xFFFF], pal: vec![0xDEAD_BEEF, 7], inner: Inner { a: 9, flag: true }, mode: Mode2::Run, maybe: Some(1 << 40), ratio: 1.5, rom: vec![9; 10] }
    }

    #[test]
    fn round_trip_and_skip() {
        let mut m = sample();
        let mut s = State::saver();
        m.sync(&mut s);
        let bytes = s.finish().unwrap();
        let mut n = Machine { rom: vec![1, 2, 3], ..Default::default() };
        let mut l = State::loader(&bytes);
        n.sync(&mut l);
        l.finish().unwrap();
        assert_eq!(n.rom, vec![1, 2, 3], "skipped fields are left alone");
        n.rom = m.rom.clone();
        assert_eq!(n, m);
    }

    #[test]
    fn whole_machine_save_and_load() {
        let mut m = sample();
        let snap = save(&mut m);
        m.pc = 1;
        m.ram.fill(0);
        load(&mut m, &snap).unwrap();
        assert_eq!((m.pc, m.ram[299 % 256]), (0x8000, (299 % 256) as u8));
        // A state of the wrong shape is refused without touching the machine.
        let mut other = Machine { ram: vec![0; 5], ..sample() };
        let before = other.pc;
        assert!(load(&mut other, &snap).is_err());
        assert_eq!(other.pc, before);
    }

    #[test]
    fn truncated_or_longer_data_is_rejected() {
        let mut m = sample();
        let mut s = State::saver();
        m.sync(&mut s);
        let mut bytes = s.finish().unwrap();
        let mut n = Machine::default();
        let mut l = State::loader(&bytes[..bytes.len() - 3]);
        n.sync(&mut l);
        assert!(l.finish().is_err());
        bytes.push(0);
        let mut l = State::loader(&bytes);
        Machine::default().sync(&mut l);
        assert!(l.finish().is_err());
    }

    #[test]
    fn delta_round_trip() {
        let a: Vec<u8> = (0..5000).map(|i| (i * 7) as u8).collect();
        let mut b = a.clone();
        b[10] ^= 0xFF;
        b[4000..4010].fill(3);
        b.extend([1, 2, 3]);
        let d = delta(&a, &b);
        assert!(d.len() < 60, "mostly-unchanged data compresses ({} bytes)", d.len());
        assert_eq!(apply(&a, &d).unwrap(), b);
        // Shrinking works too.
        let c = &a[..100];
        assert_eq!(apply(&a, &delta(&a, c)).unwrap(), c);
        assert_eq!(apply(&[], &delta(&[], &b)).unwrap(), b);
    }

    #[test]
    fn rewind_steps_back_through_history() {
        let mut r = Rewind::new(100, 1 << 20);
        let snaps: Vec<Vec<u8>> = (0..10u8).map(|i| (0..1000).map(|j| if j % 100 == 0 { i } else { 0 }).collect()).collect();
        for s in &snaps {
            r.push(s.clone());
        }
        assert_eq!(r.len(), 10);
        for i in (0..10).rev() {
            assert_eq!(r.pop().unwrap(), snaps[i]);
        }
        // The last one stays so rewinding can sit on the oldest moment.
        assert_eq!(r.pop().unwrap(), snaps[0]);
    }

    #[test]
    fn rewind_forgets_the_oldest_past_its_limit() {
        let mut r = Rewind::new(4, 1 << 20);
        for i in 0..10u8 {
            r.push(vec![i; 64]);
        }
        assert_eq!(r.len(), 4);
        let got: Vec<u8> = std::iter::from_fn(|| r.pop()).take(5).map(|v| v[0]).collect();
        assert_eq!(got, vec![9, 8, 7, 6, 6]);
    }
}

// ---------------------------------------------------------------- wall clock

/// Seconds since 1970 for cartridge clocks. A browser build has no system clock to ask, so
/// the page sets it with `set_wall_clock`; everywhere else it comes from the OS.
pub fn wall_clock_secs() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        WALL.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
    }
}

#[cfg(target_arch = "wasm32")]
static WALL: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Tells browser builds what time it is (ignored elsewhere).
pub fn set_wall_clock(secs: i64) {
    #[cfg(target_arch = "wasm32")]
    WALL.store(secs, std::sync::atomic::Ordering::Relaxed);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = secs;
}
