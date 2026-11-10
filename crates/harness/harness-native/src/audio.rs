//! The app's sound: a short cue as an agent's step changes and music that follows what is going on, mixed live to the
//! default output.
//!
//! - **Cues** are the scene's (scene/widgets/sound.flecs, `hn.sound.cue`): each a few `Sound`s in the voices
//!   harness-native-blaze synthesises, rendered once on a thread of their own through its room.
//! - **Music** is the band's ([`crate::music`]): composed and synthesised live in the callback, in the mood the scene
//!   names (`hn.sound.mood`: calm, flow, drive, battle). [`scene`], called each frame, picks the mood from what the
//!   agents are doing and how many are at it; the band glides there.
//! - **Rain** ([`music::Rain`]) plays when nothing else does, and quieter under the music, so the app is never silent.
//! - [`start`] opens the output (an AudioQueue: AudioToolbox's, linked here, no crate). With nothing sounding for a few
//!   seconds the queue pauses, and the output device rests.
//!
//! Nothing here runs unless [`start`] did: an offscreen shot and the tests make no sound.
use crate::music::{self, Band, Rain};
use harness_native_blaze::sound::{Note, soundtrack};
use harness_native_flecs::prelude::*;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// The numbers in `s`, a list in words (`"0 2 4 7 9"`), as `what` of `at`.
fn numbers<T: std::str::FromStr>(s: &ScriptString, what: &str, at: &str) -> Result<Vec<T>, String>
where
    T::Err: std::fmt::Display,
{
    s.as_str().split_whitespace().map(str::parse::<T>).collect::<Result<Vec<_>, _>>().map_err(|e| format!("{at}: {what} {:?}: {e}", s.as_str()))
}

/// An instrument of `hn.sound.inst`, as the scene writes it ([`music::Instrument`]): `partials` is pairs in words, each
/// a ratio to the note's pitch and a level (`"1 1  2 0.4  3 0.2"`).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Instrument {
    pub partials: ScriptString,
    pub attack: f32,
    pub decay: f32,
    pub hold: bool,
    pub damp: f32,
    pub fm: f32,
    pub fm_ratio: f32,
    pub fm_decay: f32,
    pub drop: f32,
    pub drop_rate: f32,
    pub noise: f32,
    pub noise_decay: f32,
    pub room: f32,
}

impl Instrument {
    /// # Errors
    /// Partials that are not pairs of numbers, or no decay: named as `at`'s.
    pub fn read(&self, at: &str) -> Result<music::Instrument, String> {
        let flat: Vec<f32> = numbers(&self.partials, "Instrument partials", at)?;
        if flat.len() % 2 != 0 || self.decay <= 0.0 {
            return Err(format!("{at}: an Instrument needs partials as pairs (a ratio and a level each) and a decay over 0"));
        }
        Ok(music::Instrument {
            partials: flat.chunks_exact(2).map(|c| (c[0], c[1])).collect(),
            attack: self.attack,
            decay: self.decay,
            hold: self.hold,
            damp: self.damp,
            fm: self.fm,
            fm_ratio: self.fm_ratio,
            fm_decay: self.fm_decay,
            drop: self.drop,
            drop_rate: self.drop_rate,
            noise: self.noise,
            noise_decay: self.noise_decay,
            room: self.room,
        })
    }
}

/// A place the rain falls (`hn.sound.ambient.*`), as the scene writes it ([`music::RainSource`]); its level is its
/// `Gain`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct RainOn {
    pub azimuth: f32,
    pub elevation: f32,
    pub distance: f32,
    pub hiss: f32,
    pub rate: f32,
    pub low: f32,
    pub high: f32,
    pub ring: f32,
    pub bend: f32,
}

/// A part of `hn.sound.part`, as the scene writes it ([`music::Part`]): `role` is chord, root, arp, motif, sparkle or
/// hit; `steps` the steps of the bar it plays on, in words (`"0 8"`). Its instrument is the `Instrument` on the same
/// entity (a part prefab inherits an instrument prefab) and its level its `Gain`.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Part {
    pub role: ScriptString,
    pub steps: ScriptString,
    pub octave: i32,
    pub pan: f32,
    pub chance: f32,
    pub beats: f32,
    pub hz: f32,
}

impl Part {
    /// The part named `name`, playing `instrument` at `level`.
    ///
    /// # Errors
    /// A role that is none of them, steps that are not 0 to 15: named as `at`'s.
    pub fn read(&self, name: String, instrument: music::Instrument, level: f32, at: &str) -> Result<music::Part, String> {
        let role = music::Role::named(self.role.as_str()).ok_or_else(|| format!("{at}: Part role {:?} is not chord, root, arp, motif, sparkle or hit", self.role.as_str()))?;
        let steps: Vec<u8> = numbers(&self.steps, "Part steps", at)?;
        if steps.iter().any(|s| *s > 15) {
            return Err(format!("{at}: Part steps {:?}: a bar's steps are 0 to 15", self.steps.as_str()));
        }
        Ok(music::Part { name, instrument: Arc::new(instrument), role, steps, octave: self.octave, level, pan: self.pan, chance: self.chance, beats: self.beats, hz: self.hz })
    }
}

/// A song of `hn.sound.song`, as the scene writes it ([`music::Song`]): `key` is semitones from `hn.sound.key`,
/// `tempo` its pace against the mood's (1: the mood's own), `bars` how long it plays; `scale`, `night`, `chords` and
/// `bridge` are lists in words. Its own parts are its children.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Song {
    pub key: i32,
    pub tempo: f32,
    pub bars: i32,
    pub scale: ScriptString,
    pub night: ScriptString,
    pub chords: ScriptString,
    pub bridge: ScriptString,
}

impl Song {
    /// The song named `name` as the band plays it, with its own `parts`.
    ///
    /// # Errors
    /// A list that is not numbers, an empty scale or no chords, a tempo or bars not over 0: named as `at`'s.
    pub fn read(&self, name: String, parts: Vec<music::Part>, at: &str) -> Result<music::Song, String> {
        let (scale, night, chords, bridge) = (numbers(&self.scale, "Song scale", at)?, numbers(&self.night, "Song night", at)?, numbers(&self.chords, "Song chords", at)?, numbers(&self.bridge, "Song bridge", at)?);
        if scale.is_empty() || chords.is_empty() || self.tempo <= 0.0 || self.bars <= 0 {
            return Err(format!("{at}: a Song needs a tempo and bars over 0, a scale and chords"));
        }
        Ok(music::Song { name, transpose: self.key, tempo: self.tempo, bars: self.bars as u32, scale, night, chords, bridge, parts })
    }
}

/// A mood of `hn.sound.mood`, as the scene writes it ([`music::Mood`]): `scale`, `night` and `chords` are lists in
/// words, all empty for a mood that plays in the song's. Its parts are its children.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Mood {
    pub tempo: f32,
    pub scale: ScriptString,
    pub night: ScriptString,
    pub chords: ScriptString,
}

impl Mood {
    /// The mood as the band plays it, with `parts`.
    ///
    /// # Errors
    /// A list that is not numbers, a scale without chords or chords without a scale, a tempo not over 0: named as
    /// `at`'s.
    pub fn read(&self, parts: Vec<music::Part>, at: &str) -> Result<music::Mood, String> {
        let (scale, night, chords) = (numbers(&self.scale, "Mood scale", at)?, numbers(&self.night, "Mood night", at)?, numbers(&self.chords, "Mood chords", at)?);
        if scale.is_empty() != chords.is_empty() || self.tempo <= 0.0 {
            return Err(format!("{at}: a Mood needs a tempo over 0, and a scale and chords together or neither (the song's)"));
        }
        Ok(music::Mood { tempo: self.tempo, scale, night, chords, parts })
    }
}

const SR: u32 = music::SR as u32;
/// Frames a buffer of the queue holds (about 21 ms).
const FRAMES: usize = 1024;
/// Seconds of nothing sounding before the queue pauses.
const REST: u64 = 3;
/// Seconds the music plays on without [`scene`] saying so again (the window stopped drawing).
const HOLD: u64 = 20;
/// How much of the rain stays under the music.
const UNDER: f32 = 0.15;
/// The moods, calmest first: [`scene`] climbs them as more is going on.
const LADDER: [&str; 4] = ["calm", "flow", "drive", "battle"];

type Buf = Arc<[[f32; 2]]>;

struct Playing {
    buf: Buf,
    at: usize,
}

struct Mixer {
    level: f32,
    /// The music's level under the master.
    music: f32,
    /// Rain's level under the master when nothing else plays (0: none); under the music it is [`UNDER`] of it.
    ambient: f32,
    rain: Rain,
    playing: Vec<Playing>,
    band: Band,
    /// What the band plays when no agent works (the scene's `rest` mood), and whether it is night.
    rest: Option<music::Mood>,
    night: bool,
    /// The frame the music goes back to `rest` at unless a mood is asked for again.
    until: u64,
    clock: u64,
    idle: u64,
    paused: bool,
}

impl Mixer {
    fn render(&mut self, out: &mut [[f32; 2]]) {
        if self.clock >= self.until {
            self.band.set(self.rest.as_ref(), self.night);
        }
        let mut band = [[0.0f32; 2]; FRAMES];
        let n = out.len().min(FRAMES);
        self.band.render(&mut band[..n]);
        self.rain.set(self.ambient * self.level * if self.band.playing() { UNDER } else { 1.0 });
        self.rain.render(out);
        for (o, b) in out.iter_mut().zip(&band[..n]) {
            o[0] += b[0] * self.music * self.level;
            o[1] += b[1] * self.music * self.level;
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
        self.idle = if self.playing.is_empty() && !self.band.playing() && !self.rain.playing() { self.idle + out.len() as u64 } else { 0 };
        self.clock += out.len() as u64;
    }
}

static MIXER: OnceLock<Mutex<Mixer>> = OnceLock::new();
static CUES: OnceLock<Mutex<HashMap<String, Buf>>> = OnceLock::new();
static MOODS: OnceLock<HashMap<String, music::Mood>> = OnceLock::new();
static QUEUE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
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
    // The lock is let go before any call into the queue: the queue's calls can wait on this callback.
    let rest = MIXER.get().is_some_and(|m| {
        let mut m = m.lock().unwrap_or_else(PoisonError::into_inner);
        m.render(out);
        let rest = m.idle > REST * u64::from(SR) && !m.paused;
        m.paused |= rest;
        rest
    });
    if rest {
        // SAFETY: the queue this callback was called for.
        unsafe { AudioQueuePause(queue) };
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

/// A cue of the scene: its name, its level and its notes.
pub struct Cue(pub String, pub f32, pub Vec<Note>);

/// Open the output at `level` (the scene's master) with the music at `music` under it (0: none), render `cues` on a
/// thread of their own, and keep `moods` for [`scene`]; the band plays `songs` one after another, starting on a
/// different one each launch. `ambient`: the rain's level (0: none) and `rain` the
/// places it falls; with rain the output never rests.
///
/// # Errors
/// The system refused the queue or a buffer (its status, named).
pub fn start(level: f32, music: f32, ambient: f32, rain: &[music::RainSource], cues: Vec<Cue>, moods: HashMap<String, music::Mood>, songs: Vec<music::Song>) -> Result<(), String> {
    if MIXER.get().is_some() {
        return Ok(());
    }
    let _ = CUES.set(Mutex::new(HashMap::new()));
    let rest = moods.get("rest").cloned();
    let _ = MOODS.set(moods);
    // The first song differs by launch: the band's seed is the time.
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
    let mut band = Band::new(seed);
    band.set_songs(songs);
    let _ = MIXER.set(Mutex::new(Mixer { level, music, ambient, rain: Rain::new(rain), playing: Vec::new(), band, rest: None, night: false, until: 0, clock: 0, idle: 0, paused: false }));
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
    if let Some(m) = MIXER.get() {
        m.lock().unwrap_or_else(PoisonError::into_inner).rest = rest;
    }
    QUEUE.store(queue, Ordering::Release);
    // Paused until something plays; with rain, something always does.
    if (ambient > 0.0 && !rain.is_empty()) || MOODS.get().is_some_and(|m| m.contains_key("rest")) {
        // SAFETY: the queue just made.
        unsafe { AudioQueueStart(queue, std::ptr::null()) };
    } else if let Some(m) = MIXER.get() {
        m.lock().unwrap_or_else(PoisonError::into_inner).paused = true;
    }
    std::thread::Builder::new()
        .name("harness-sound".to_owned())
        .spawn(move || {
            for (i, Cue(name, gain, notes)) in cues.into_iter().enumerate() {
                let buf = render(&notes, gain, 7 + i as u64);
                if let Some(l) = CUES.get() {
                    l.lock().unwrap_or_else(PoisonError::into_inner).insert(name, buf); // alloc: the library, kept for the process
                }
            }
            // Everything is rendered: one bell says the sound is ready.
            cue("ready");
        })
        .map_err(|e| format!("the sound thread: {e}"))?;
    Ok(())
}

/// The mixer, with its queue running again if it had paused.
fn with_mixer(f: impl FnOnce(&mut Mixer)) {
    let Some(m) = MIXER.get() else { return };
    let queue = QUEUE.load(Ordering::Acquire);
    let wake = {
        let mut m = m.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut m);
        let wake = m.paused && !queue.is_null() && (!m.playing.is_empty() || m.band.playing());
        if wake {
            m.paused = false;
            m.idle = 0;
        }
        wake
    };
    // Started with the lock let go: AudioQueueStart can call the callback, which takes it.
    if wake {
        // SAFETY: the queue start made, never disposed.
        unsafe { AudioQueueStart(queue, std::ptr::null()) };
    }
}

/// Play cue `name` (nothing before [`start`], or while it is still being rendered).
pub fn cue(name: &str) {
    let Some(buf) = CUES.get().and_then(|l| l.lock().unwrap_or_else(PoisonError::into_inner).get(name).cloned()) else { return };
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

/// How far up the [`LADDER`] the music goes: by the step the chat in front is on (thinking, reading and waiting are
/// calm; editing, searching, querying and fetching flow; a command, a build or a new agent drives), a rung more with
/// three agents at work at once and another with six.
fn rung(item: &str, agents: usize) -> usize {
    let by_step = match item {
        "edit" | "search" | "sql" | "net" => 1,
        "exec" | "build" | "spawn" => 2,
        _ => 0,
    };
    (by_step + usize::from(agents >= 3) + usize::from(agents >= 6)).min(LADDER.len() - 1)
}

/// Each frame: how many agents are at work, whether the chat in front is one of them, and whether it is night. While
/// any works the music plays in the mood that fits ([`rung`]); when the chat in front stops, `done` rings (`fail` when it ended
/// on an error); with none at
/// work the band goes back to its `rest` mood.
pub fn scene(agents: usize, front: bool, failed: bool, night: bool) {
    if MIXER.get().is_none() {
        return;
    }
    let mut now = NOW.lock().unwrap_or_else(PoisonError::into_inner);
    let (was, item) = (now.1, now.0.clone());
    now.1 = front;
    if !front {
        now.0.clear();
    }
    drop(now);
    if was && !front {
        cue(if failed { "fail" } else { "done" });
    }
    let mood = (agents > 0).then(|| MOODS.get().and_then(|m| m.get(LADDER[rung(&item, agents)]))).flatten();
    with_mixer(|m| {
        m.night = night;
        match mood {
            Some(mood) => {
                m.band.set(Some(mood), night);
                m.until = m.clock + HOLD * u64::from(SR);
            }
            // No agent at work: the rest mood (ambient music), from the next callback.
            None => m.until = 0,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// More going on is a higher rung, never past the top.
    #[test]
    fn the_mood_climbs_with_the_work() {
        assert_eq!([rung("think", 1), rung("edit", 1), rung("exec", 1), rung("exec", 3), rung("exec", 9)], [0, 1, 2, 3, 3]);
        assert_eq!(rung("", 4), 1);
    }
}

#[cfg(test)]
mod live {
    use super::*;
    use harness_native_blaze::sound::Voice;
    /// By hand (`--ignored`): opens the real output, plays one cue, then the band for a few seconds.
    #[test]
    #[ignore]
    fn plays_out_loud() {
        let n = |at: f32, hz: f32| Note { voice: Voice::Mallet, at, dur: 1.0, freq: hz, amp: 0.05, pan: 0.0, sweep: 0.0 };
        let inst = |hold: bool| Arc::new(music::Instrument { partials: vec![(1.0, 1.0), (2.0, 0.4)], attack: if hold { 1.0 } else { 0.003 }, decay: if hold { 1.4 } else { 3.0 }, hold, damp: 1.2, room: 0.3, ..music::Instrument::default() });
        let part = |name: &str, role, steps: &[u8], level, hold| music::Part { name: name.to_owned(), instrument: inst(hold), role, steps: steps.to_vec(), octave: 1, level, pan: 0.0, chance: 1.0, beats: 4.0, hz: 60.0 };
        let flow = music::Mood { tempo: 84.0, scale: vec![0, 2, 4, 7, 9], night: vec![], chords: vec![0, 3, 2, 4], parts: vec![part("pad", music::Role::Chord, &[0], 0.08, true), part("lead", music::Role::Motif, &[], 0.15, false)] };
        start(1.0, 0.6, 0.08, &[], vec![Cue("done".into(), 0.3, vec![n(0.0, 440.0), n(0.15, 554.37), n(0.3, 659.25)])], HashMap::from([("flow".to_owned(), flow)]), Vec::new()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        cue("done");
        for _ in 0..80 {
            scene(1, true, false, false);
            step("edit");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let g = MIXER.get().unwrap().lock().unwrap();
        println!("clock {} paused {} band {}", g.clock, g.paused, g.band.playing());
    }
}
