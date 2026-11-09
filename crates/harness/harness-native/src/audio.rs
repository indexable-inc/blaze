//! The app's sound: a short cue as an agent's step changes and a quiet music that follows what it is doing, mixed live
//! to the default output.
//!
//! What plays is the scene's (scene/widgets/sound.flecs, `hn.sound`): each cue a few `Sound`s (the voices
//! harness-native-blaze synthesises: a bell, a mallet, a click, a thud, a whoosh), each theme a voice, a tempo and the
//! degrees it plays over one key. Every theme is in that key, so moving from one to the next is only a change of
//! register, pace and instrument: the notes still ringing belong to both.
//!
//! - [`start`] opens the output (an AudioQueue: AudioToolbox's, linked here, no crate) and renders every cue and every
//!   theme's notes once, on a thread of its own, each through the soundtrack's room (its reverb).
//! - [`step`] says the step a running turn is on; a new one plays its cue. [`working`], called each frame, keeps the
//!   theme of that step playing while the chat in front works and plays `done` when it stops.
//! - The mixer runs in the queue's callback: it starts the theme's notes itself (a walk over its degrees, a few note
//!   lengths), so the music keeps time whatever the window draws. With nothing sounding for a few seconds the queue
//!   pauses, and the output device rests.
//!
//! Nothing here runs unless [`start`] did: an offscreen shot and the tests make no sound.
use harness_native_blaze::sound::{Note, soundtrack};
use harness_native_flecs::prelude::*;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// A theme of `hn.sound.theme`: a note about every `every` seconds, from `degrees` (semitones over the key, a list in
/// words: `"0 7 12 16"`). Its voice, how long a note rings and its level are its `Sound`, `Lasts` and `Gain`.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Theme {
    pub every: f32,
    pub degrees: ScriptString,
}

const SR: u32 = 48_000;
/// Frames a buffer of the queue holds (about 21 ms).
const FRAMES: usize = 1024;
/// Seconds of nothing sounding before the queue pauses.
const REST: u64 = 3;
/// Seconds a theme plays on without [`working`] saying so again (the window stopped drawing).
const HOLD: u64 = 20;

type Buf = Arc<[[f32; 2]]>;

/// A theme as the mixer plays it: its notes, low to high, and the seconds between them.
struct Music {
    every: f32,
    notes: Vec<Buf>,
}

struct Playing {
    buf: Buf,
    at: usize,
}

struct Mixer {
    level: f32,
    playing: Vec<Playing>,
    music: Option<Arc<Music>>,
    /// The frame the theme stops at unless it is asked for again.
    until: u64,
    clock: u64,
    /// The frame the theme's next note starts at, the note it last played and its walk's state.
    next: u64,
    at: usize,
    rng: u64,
    idle: u64,
    paused: bool,
}

impl Mixer {
    fn render(&mut self, out: &mut [[f32; 2]]) {
        if let Some(mu) = self.music.clone() {
            if self.clock >= self.until {
                self.music = None;
            } else if self.clock >= self.next && !mu.notes.is_empty() {
                self.rng ^= self.rng << 13;
                self.rng ^= self.rng >> 7;
                self.rng ^= self.rng << 17;
                let r = (self.rng >> 24) as usize;
                // A walk: a step or two up or down the theme's notes, so it wanders and never leaps.
                let by = [-2i64, -1, -1, 1, 1, 2][r % 6];
                self.at = (self.at as i64 + by).rem_euclid(mu.notes.len() as i64) as usize;
                self.playing.push(Playing { buf: mu.notes[self.at].clone(), at: 0 }); // alloc: a handful sounding at once
                let beats = [1.0f32, 1.0, 0.5, 1.5, 2.0][(r >> 8) % 5];
                self.next = self.clock + (mu.every * beats * SR as f32) as u64;
            }
        }
        for p in &mut self.playing {
            for (o, s) in out.iter_mut().zip(&p.buf[p.at..]) {
                o[0] += s[0] * self.level;
                o[1] += s[1] * self.level;
            }
            p.at += out.len();
        }
        self.playing.retain(|p| p.at < p.buf.len());
        for o in out.iter_mut() {
            *o = o.map(f32::tanh);
        }
        self.idle = if self.playing.is_empty() && self.music.is_none() { self.idle + out.len() as u64 } else { 0 };
        self.clock += out.len() as u64;
    }
}

/// What was rendered: each cue's sound and each theme's music, by name.
#[derive(Default)]
struct Lib {
    cues: HashMap<String, Buf>,
    themes: HashMap<String, Arc<Music>>,
}

static MIXER: OnceLock<Mutex<Mixer>> = OnceLock::new();
static LIB: OnceLock<Mutex<Lib>> = OnceLock::new();
static QUEUE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static MUSIC: AtomicBool = AtomicBool::new(true);
/// The step the running turn is on, and whether the chat in front was working last frame.
static NOW: Mutex<(String, bool)> = Mutex::new((String::new(), false));

#[repr(C)]
struct Format {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct QueueBuffer {
    capacity: u32,
    data: *mut c_void,
    byte_size: u32,
    user: *mut c_void,
    packet_capacity: u32,
    packets: *mut c_void,
    packet_count: u32,
}

type Callback = extern "C" fn(*mut c_void, *mut c_void, *mut QueueBuffer);

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewOutput(format: *const Format, callback: Callback, user: *mut c_void, run_loop: *const c_void, mode: *const c_void, flags: u32, out: *mut *mut c_void) -> i32;
    fn AudioQueueAllocateBuffer(queue: *mut c_void, bytes: u32, out: *mut *mut QueueBuffer) -> i32;
    fn AudioQueueEnqueueBuffer(queue: *mut c_void, buffer: *mut QueueBuffer, packets: u32, descriptions: *const c_void) -> i32;
    fn AudioQueueStart(queue: *mut c_void, at: *const c_void) -> i32;
    fn AudioQueuePause(queue: *mut c_void) -> i32;
}

/// The queue's callback, on its own thread: the next buffer, mixed.
extern "C" fn fill(_user: *mut c_void, queue: *mut c_void, buffer: *mut QueueBuffer) {
    // SAFETY: the queue hands back a buffer it allocated with room for FRAMES stereo f32 frames (start), aligned as
    // malloc aligns; nothing else touches it until it is enqueued again below.
    let (b, out) = unsafe {
        let b = &mut *buffer;
        (b as *mut QueueBuffer, std::slice::from_raw_parts_mut(b.data.cast::<[f32; 2]>(), FRAMES))
    };
    out.fill([0.0; 2]);
    if let Some(m) = MIXER.get() {
        let mut m = m.lock().unwrap_or_else(PoisonError::into_inner);
        m.render(out);
        if m.idle > REST * u64::from(SR) && !m.paused {
            m.paused = true;
            // SAFETY: the queue this callback was called for.
            unsafe { AudioQueuePause(queue) };
        }
    }
    // SAFETY: the buffer is this queue's, filled above.
    unsafe {
        (*b).byte_size = (FRAMES * 8) as u32;
        AudioQueueEnqueueBuffer(queue, b, 0, std::ptr::null());
    }
}

/// One sound of the scene, rendered alone through the soundtrack's room, at `gain`.
fn render(notes: &[Note], gain: f32, seed: u64) -> Buf {
    let seconds = notes.iter().map(|n| n.at + n.dur.max(0.1)).fold(0.0, f32::max) + 1.6;
    soundtrack(SR, seconds, None, &[], notes, seed).into_iter().map(|f| f.map(|s| s * gain)).collect()
}

/// A cue of the scene: its name, its level and its notes. A theme: its name, its seconds between notes, its level and
/// its notes, low to high (one a degree).
pub struct Cue(pub String, pub f32, pub Vec<Note>);
pub struct Tune(pub String, pub f32, pub f32, pub Vec<Note>);

/// Open the output at `level` (the scene's master) and render `cues` and `themes` on a thread of their own; `music`:
/// whether themes play at all.
///
/// # Errors
/// The system refused the queue or a buffer (its status, named).
pub fn start(level: f32, music: bool, cues: Vec<Cue>, themes: Vec<Tune>) -> Result<(), String> {
    if MIXER.get().is_some() {
        return Ok(());
    }
    MUSIC.store(music, Ordering::Relaxed);
    let _ = LIB.set(Mutex::new(Lib::default()));
    let mixer = Mixer { level, playing: Vec::new(), music: None, until: 0, clock: 0, next: 0, at: 0, rng: 0x9e37_79b9_7f4a_7c15, idle: 0, paused: false };
    let _ = MIXER.set(Mutex::new(mixer));
    // 32-bit float, interleaved stereo ('lpcm'; float | packed).
    let format = Format { sample_rate: f64::from(SR), format_id: 0x6C70_636D, format_flags: 1 | 8, bytes_per_packet: 8, frames_per_packet: 1, bytes_per_frame: 8, channels_per_frame: 2, bits_per_channel: 32, reserved: 0 };
    let mut queue = std::ptr::null_mut();
    // SAFETY: a valid format and callback; a null run loop has the queue call back on its own thread.
    let status = unsafe { AudioQueueNewOutput(&format, fill, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), 0, &mut queue) };
    if status != 0 || queue.is_null() {
        return Err(format!("AudioQueueNewOutput: status {status}"));
    }
    for _ in 0..3 {
        let mut b = std::ptr::null_mut();
        // SAFETY: the queue just made; the buffer it allocates is primed by the callback itself.
        let status = unsafe { AudioQueueAllocateBuffer(queue, (FRAMES * 8) as u32, &mut b) };
        if status != 0 || b.is_null() {
            return Err(format!("AudioQueueAllocateBuffer: status {status}"));
        }
        fill(std::ptr::null_mut(), queue, b);
    }
    QUEUE.store(queue, Ordering::Release);
    // Paused until something plays.
    if let Some(m) = MIXER.get() {
        m.lock().unwrap_or_else(PoisonError::into_inner).paused = true;
    }
    std::thread::Builder::new()
        .name("harness-sound".to_owned())
        .spawn(move || {
            let lib = || LIB.get().map(|l| l.lock().unwrap_or_else(PoisonError::into_inner));
            for (i, Cue(name, gain, notes)) in cues.into_iter().enumerate() {
                let buf = render(&notes, gain, 7 + i as u64);
                if let Some(mut l) = lib() {
                    l.cues.insert(name, buf); // alloc: the library, kept for the process
                }
            }
            for (i, Tune(name, every, gain, notes)) in themes.into_iter().enumerate() {
                let notes = notes.iter().enumerate().map(|(k, n)| render(std::slice::from_ref(n), gain, 101 + (i * 16 + k) as u64)).collect();
                if let Some(mut l) = lib() {
                    l.themes.insert(name, Arc::new(Music { every, notes })); // alloc: the library, kept for the process
                }
            }
        })
        .map_err(|e| format!("the sound thread: {e}"))?;
    Ok(())
}

/// The mixer, with its queue running again if it had paused.
fn with_mixer(f: impl FnOnce(&mut Mixer)) {
    let Some(m) = MIXER.get() else { return };
    let mut m = m.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut m);
    let queue = QUEUE.load(Ordering::Acquire);
    if m.paused && !queue.is_null() && (!m.playing.is_empty() || m.music.is_some()) {
        m.paused = false;
        m.idle = 0;
        // SAFETY: the queue start made, never disposed.
        unsafe { AudioQueueStart(queue, std::ptr::null()) };
    }
}

/// Play cue `name` (nothing before [`start`], or while it is still being rendered).
pub fn cue(name: &str) {
    let Some(buf) = LIB.get().and_then(|l| l.lock().unwrap_or_else(PoisonError::into_inner).cues.get(name).cloned()) else { return };
    with_mixer(|m| m.playing.push(Playing { buf, at: 0 })); // alloc: a handful sounding at once
}

/// The running turn is on a step of kind `kind` (steps.rs's, as its block shows it): a new one plays its cue.
pub fn step(kind: &str) {
    if MIXER.get().is_none() {
        return;
    }
    let item = harness_native_render::block::item(kind);
    let mut now = NOW.lock().unwrap_or_else(PoisonError::into_inner);
    if now.0 != item {
        item.clone_into(&mut now.0);
        drop(now);
        cue(item);
    }
}

/// Each frame: whether the chat in front is working. While it is, the theme of its step plays; when it stops, the
/// music rings out and `done` plays.
pub fn working(busy: bool) {
    if MIXER.get().is_none() {
        return;
    }
    let mut now = NOW.lock().unwrap_or_else(PoisonError::into_inner);
    let (was, kind) = (now.1, now.0.clone());
    now.1 = busy;
    if !busy {
        now.0.clear();
    }
    drop(now);
    if busy && MUSIC.load(Ordering::Relaxed) {
        let name = if kind.is_empty() { "think" } else { kind.as_str() };
        let theme = LIB.get().and_then(|l| l.lock().unwrap_or_else(PoisonError::into_inner).themes.get(name).cloned());
        with_mixer(|m| {
            if let Some(t) = theme
                && !m.music.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, &t))
            {
                // The next theme takes over at the beat the last one would have played.
                m.at = m.at.min(t.notes.len().saturating_sub(1));
                m.music = Some(t);
            }
            m.until = m.clock + HOLD * u64::from(SR);
        });
    } else if was && !busy {
        with_mixer(|m| m.music = None);
        cue("done");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_native_blaze::sound::Voice;

    /// A theme's notes start on their own in the mixer, a new one about every `every` seconds, the mix stays under
    /// full scale, and once the theme is let go and its notes have rung out the mixer counts itself idle.
    #[test]
    fn a_theme_plays_itself_and_rings_out() {
        let note = |hz: f32| render(&[Note { voice: Voice::Mallet, at: 0.0, dur: 1.0, freq: hz, amp: 0.05, pan: 0.0, sweep: 0.0 }], 0.05, 1);
        let music = Arc::new(Music { every: 0.5, notes: vec![note(220.0), note(330.0), note(440.0)] });
        assert!(music.notes[0].iter().flatten().any(|s| s.abs() > 0.001), "a rendered note is not silent");
        let mut m = Mixer { level: 0.5, playing: Vec::new(), music: Some(music), until: 10 * u64::from(SR), clock: 0, next: 0, at: 0, rng: 7, idle: 0, paused: false };
        let (mut peak, mut started) = (0.0f32, 0usize);
        let mut out = [[0.0f32; 2]; FRAMES];
        for _ in 0..(4 * SR as usize / FRAMES) {
            let before = m.playing.len();
            out.fill([0.0; 2]);
            m.render(&mut out);
            started += usize::from(m.playing.len() > before);
            peak = out.iter().flatten().fold(peak, |p, s| p.max(s.abs()));
        }
        assert!((3..=16).contains(&started), "{started} notes started in 4 s at one about every 0.5 s");
        assert!(peak > 0.0005 && peak < 0.5, "peak {peak}");
        m.music = None;
        for _ in 0..(8 * SR as usize / FRAMES) {
            out.fill([0.0; 2]);
            m.render(&mut out);
        }
        assert!(m.playing.is_empty() && m.idle > u64::from(SR), "rung out and idle");
    }
}
