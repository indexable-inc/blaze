//! The app's music: a small band that plays itself and follows a mood, a game's adaptive score. It is synthesised
//! and composed live, sample by sample, in the audio callback ([`crate::audio`]); nothing is a recording or a loop.
//!
//! Everything the band is made of is the scene's (scene/widgets/sound.flecs, read by [`crate::audio`]):
//! - An [`Instrument`] is a recipe for one note: partials and how fast each dies, an attack, a decay or a held note
//!   with a release, optional FM (a bell), a falling pitch (a drum) and noise (a hat). `hn.sound.inst.*` prefabs.
//! - A [`Part`] is an instrument with a role in the bar: hold the chord, play its root, climb it, play the lead's
//!   motif, ring now and then, or hit on its steps. `hn.sound.part.*` prefabs, each inheriting its instrument.
//! - A [`Mood`] is a tempo, a scale by day and one by night, a chord a bar, and the parts that play in it, each at a
//!   level. `hn.sound.mood.*`, each a few part prefabs' instances.
//! - A [`Song`] is a key, a scale by day and by night, chords and a few parts of its own (a flute for the lead). The
//!   band plays one for its bars, goes quiet a few bars, then plays another. `hn.sound.song.*`.
//!
//! A new mood never cuts: every part's level glides to its level in the new mood (a part the new mood lacks fades out,
//! one it adds fades in), the tempo glides, and the scale and chords take over on the next bar line, where every part
//! changes together. With no mood the band fades out, rings out and rests.
//!
//! No IO and no clock but the samples asked for: [`Band::render`] is the whole of it, so a test plays it offline.
use std::f32::consts::TAU;
use std::sync::Arc;

/// Samples a second the band is rendered at.
pub const SR: f32 = 48_000.0;
/// Steps (sixteenths) a bar.
const STEPS: usize = 16;
/// The key's root: A2.
const ROOT: f32 = 110.0;
/// The most voices sounding at once (the oldest gives way).
const VOICES: usize = 48;

/// A recipe for one note.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Instrument {
    /// Each partial: its ratio to the note's pitch and its level.
    pub partials: Vec<(f32, f32)>,
    /// Seconds to full level.
    pub attack: f32,
    /// How fast the note dies (1/s) from its start; with `hold`, from its release.
    pub decay: f32,
    /// The note is held for its length, then dies (a pad); else it dies from the start (a pluck).
    pub hold: bool,
    /// How much faster each higher partial dies (1/s a partial): a plucked string's brightness going first.
    pub damp: f32,
    /// FM: how far partial phases are bent at the onset, by a sine at `fm_ratio` of the pitch, dying at `fm_decay`.
    pub fm: f32,
    pub fm_ratio: f32,
    pub fm_decay: f32,
    /// A pitch that falls in: Hz over the note's at its onset, falling at `drop_rate` (1/s): a drum's skin.
    pub drop: f32,
    pub drop_rate: f32,
    /// Noise with its lows taken out, at this level, dying at `noise_decay` (1/s): a hat, a stick.
    pub noise: f32,
    pub noise_decay: f32,
    /// How much of it goes to the room.
    pub room: f32,
}

/// What a part does in the bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The bar's chord (its root and the notes two and four steps of the scale over it), all three, on its steps.
    Chord,
    /// The chord's root.
    Root,
    /// Up through the chord over two octaves, a note a step of its.
    Arp,
    /// The lead: the band's motif (two bars it repeats and varies), over the chord's root; its steps are the motif's.
    Motif,
    /// A note of the chord now and then (`chance`), on its steps.
    Sparkle,
    /// A hit at its own pitch (`hz`), on its steps: a drum.
    Hit,
}

impl Role {
    #[must_use]
    pub fn named(s: &str) -> Option<Self> {
        Some(match s {
            "chord" => Self::Chord,
            "root" => Self::Root,
            "arp" => Self::Arp,
            "motif" => Self::Motif,
            "sparkle" => Self::Sparkle,
            "hit" => Self::Hit,
            _ => return None,
        })
    }
}

/// An instrument's role in the bar.
#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    /// What it goes by across moods: the same name in the next mood is the same player, its level gliding.
    pub name: String,
    pub instrument: Arc<Instrument>,
    pub role: Role,
    /// The steps of the bar (0 to 15) it plays on.
    pub steps: Vec<u8>,
    /// Octaves over the key's root.
    pub octave: i32,
    pub level: f32,
    pub pan: f32,
    /// How likely each of its steps sounds (1: always).
    pub chance: f32,
    /// Beats a held note lasts.
    pub beats: f32,
    /// A [`Role::Hit`]'s pitch.
    pub hz: f32,
}

/// A song: a piece the band plays for a while before it moves to another. It gives the key (`transpose` semitones from
/// A), the scale by day and by night, the chords (a root a bar, as steps of the scale: `chords` for eight bars, then
/// `bridge` for eight, and round again), how fast it goes against the mood's tempo, how many bars it lasts, and any
/// parts of its own: one named like a mood's part plays in its place, at the mood's level (a flute for the lead).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Song {
    pub name: String,
    pub transpose: i32,
    pub tempo: f32,
    pub bars: u32,
    pub scale: Vec<i32>,
    pub night: Vec<i32>,
    pub chords: Vec<i32>,
    pub bridge: Vec<i32>,
    pub parts: Vec<Part>,
}

/// Bars of quiet between one song and the next (the band fades out, rests, and the next song fades in).
const BETWEEN: u32 = 4;

/// A mood: its tempo (quarter notes a minute), its parts, and, when it has them, a scale (semitones over the key) by day
/// and by night and chords of its own, which it plays whatever the song (a battle's minor); without, the song's.
#[derive(Clone, Debug, PartialEq)]
pub struct Mood {
    pub tempo: f32,
    pub scale: Vec<i32>,
    pub night: Vec<i32>,
    pub chords: Vec<i32>,
    pub parts: Vec<Part>,
}

struct Voice {
    inst: Arc<Instrument>,
    freq: f32,
    amp: f32,
    pan: f32,
    /// Samples played, and how many a held note holds before its release.
    t: u32,
    hold: u32,
    phase: f32,
    last: f32,
}

impl Voice {
    /// The voice's next sample and whether it has ended.
    fn next(&mut self, noise: f32) -> (f32, bool) {
        let i = &*self.inst;
        let t = self.t as f32 / SR;
        self.t += 1;
        self.phase += TAU * (self.freq + i.drop * (-t * i.drop_rate).exp()) / SR;
        if self.phase > TAU * 4096.0 {
            self.phase -= TAU * 4096.0;
        }
        let bend = if i.fm > 0.0 { i.fm * (-t * i.fm_decay).exp() * (self.phase * i.fm_ratio).sin() } else { 0.0 };
        let mut s = 0.0;
        for (k, (ratio, level)) in i.partials.iter().enumerate() {
            let dies = if i.damp > 0.0 { (-t * i.damp * k as f32).exp() } else { 1.0 };
            s += level * dies * (self.phase * ratio + bend).sin();
        }
        if i.noise > 0.0 {
            s += i.noise * (-t * i.noise_decay).exp() * (noise - self.last);
            self.last = noise;
        }
        let held = self.hold as f32 / SR;
        let since = if i.hold { (t - held).max(0.0) } else { t };
        let env = (t / i.attack.max(0.0005)).min(1.0) * (-since * i.decay).exp();
        (s * env, since * i.decay.max(0.05) > 7.0)
    }
}

/// A room: four combs and two all-passes a side (Schroeder's), the right side's a little longer.
struct Room {
    combs: [[Vec<f32>; 4]; 2],
    damp: [[f32; 4]; 2],
    passes: [[Vec<f32>; 2]; 2],
    at: usize,
}

impl Room {
    fn new() -> Self {
        let line = |n: usize| vec![0.0; n]; // alloc: the room's delay lines, made once
        let side = |d: usize| [line(1687 + d), line(1601 + d), line(2053 + d), line(2251 + d)];
        let pass = |d: usize| [line(347 + d), line(113 + d)];
        Self { combs: [side(0), side(37)], damp: [[0.0; 4]; 2], passes: [pass(0), pass(11)], at: 0 }
    }

    fn next(&mut self, x: f32) -> [f32; 2] {
        self.at = self.at.wrapping_add(1);
        let mut out = [0.0; 2];
        for (side, o) in out.iter_mut().enumerate() {
            let mut s = 0.0;
            for (k, line) in self.combs[side].iter_mut().enumerate() {
                let i = self.at % line.len();
                let y = line[i];
                self.damp[side][k] = y * 0.72 + self.damp[side][k] * 0.28;
                line[i] = x + self.damp[side][k] * 0.86;
                s += y;
            }
            for line in &mut self.passes[side] {
                let i = self.at % line.len();
                let y = line[i];
                line[i] = s + y * 0.5;
                s = y - s * 0.5;
            }
            *o = s * 0.2;
        }
        out
    }
}

/// One place the rain falls, heard from where the listener sits (the scene's `hn.sound.ambient.*`): where it is, and
/// what the rain sounds like on it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RainSource {
    /// Degrees round from straight ahead (positive: to the right; 180: behind), degrees up from the ears' level
    /// (90: overhead), and how far, in metres.
    pub azimuth: f32,
    pub elevation: f32,
    pub distance: f32,
    pub gain: f32,
    /// The steady wash of rain on it: its level, between `low` and `high` Hz.
    pub hiss: f32,
    /// Single drops: how many a second, each ringing at a pitch between `low` and `high`, dying at `ring` (1/s: a
    /// dull roof high, glass higher, water low), its pitch bending by `bend` of itself as it rings (a drip into water
    /// rises).
    pub rate: f32,
    pub low: f32,
    pub high: f32,
    pub ring: f32,
    pub bend: f32,
}

/// The longest an ear waits for a sound the other heard first, in samples (about 0.7 ms: a head's width).
const HEAD: usize = 34;

/// A source as it plays: its drops, its wash's filters, and the last samples it made, for the later ear.
struct Falling {
    at: RainSource,
    fast: f32,
    slow: f32,
    /// Drops sounding: each its pitch and its samples so far.
    drops: [(f32, u32); 6],
    line: [f32; 64],
    head: usize,
    /// Each ear: how many samples late it hears the source, how loud, and how much of its highs the head keeps.
    ears: [(usize, f32, f32); 2],
    shade: [f32; 2],
    /// How much goes to the room: more the farther it is.
    room: f32,
}

impl Falling {
    fn new(at: RainSource) -> Self {
        let (az, el) = (at.azimuth.to_radians(), at.elevation.to_radians());
        // How far to the right it is, -1 to 1: nothing overhead, all of it at the side.
        let side = az.sin() * el.cos();
        let behind = (az.cos() * el.cos()).min(0.0).abs();
        let near = 1.0 / at.distance.max(0.5);
        let ear = |right: bool| {
            let toward = if right { side } else { -side };
            // The far ear hears it later, quieter, and duller (the head's shadow); from behind both are a little duller.
            let late = if toward < 0.0 { (-toward * HEAD as f32) as usize } else { 0 };
            let level = near * (0.62 + 0.38 * toward);
            let open = (0.85 - 0.6 * (-toward).max(0.0) - 0.25 * behind - 0.03 * at.distance).clamp(0.08, 1.0);
            (late, level, open)
        };
        Self { at, fast: 0.0, slow: 0.0, drops: [(0.0, u32::MAX); 6], line: [0.0; 64], head: 0, ears: [ear(false), ear(true)], shade: [0.0; 2], room: (0.08 * at.distance).min(0.6) }
    }

    /// Its next sample in each ear, and what it sends to the room.
    fn next(&mut self, noise: f32, chance: f32, pick: f32) -> ([f32; 2], f32) {
        let a = self.at;
        let pole = |hz: f32| 1.0 - (-TAU * hz / SR).exp();
        self.fast += (noise - self.fast) * pole(a.high.max(a.low + 1.0));
        self.slow += (noise - self.slow) * pole(a.low.max(20.0));
        let mut s = a.hiss * (self.fast - self.slow);
        if chance < a.rate / SR
            && let Some(free) = self.drops.iter().position(|d| d.1 == u32::MAX)
        {
            self.drops[free] = (a.low + (a.high - a.low) * pick, 0);
        }
        for d in &mut self.drops {
            if d.1 == u32::MAX {
                continue;
            }
            let t = d.1 as f32 / SR;
            let dies = (-t * a.ring).exp();
            s += 0.5 * dies * (TAU * d.0 * (1.0 + a.bend * (1.0 - dies)) * t).sin();
            d.1 = if dies < 0.002 { u32::MAX } else { d.1 + 1 };
        }
        let s = s * a.gain;
        self.head = (self.head + 1) % self.line.len();
        self.line[self.head] = s;
        let mut out = [0.0; 2];
        for (side, o) in out.iter_mut().enumerate() {
            let (late, level, open) = self.ears[side];
            let heard = self.line[(self.head + self.line.len() - late) % self.line.len()];
            self.shade[side] += (heard - self.shade[side]) * open;
            *o = self.shade[side] * level;
        }
        (out, s * self.room)
    }
}

/// Rain in a room, for when nothing else plays (and, far quieter, under the music): each place it falls a source of
/// its own ([`RainSource`]), placed round the listener by ear (the later, quieter, duller far ear: on headphones it
/// comes from there), the farther ones more in the room.
pub struct Rain {
    rng: u64,
    sources: Vec<Falling>,
    room: Room,
    level: f32,
    want: f32,
}

impl Rain {
    #[must_use]
    pub fn new(sources: &[RainSource]) -> Self {
        Self { rng: 0x9e37_79b9_7f4a_7c15, sources: sources.iter().map(|s| Falling::new(*s)).collect(), room: Room::new(), level: 0.0, want: 0.0 }
    }

    /// Rain at `level` from here on (it comes and goes over a few seconds).
    pub fn set(&mut self, level: f32) {
        self.want = level;
    }

    #[must_use]
    pub fn playing(&self) -> bool {
        !self.sources.is_empty() && (self.want > 0.0 || self.level > 0.0005)
    }

    fn rand(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Add the rain's next `out.len()` frames to `out`.
    pub fn render(&mut self, out: &mut [[f32; 2]]) {
        if !self.playing() {
            return;
        }
        for frame in out {
            self.level += (self.want - self.level) * 0.000_02;
            let (mut ears, mut send) = ([0.0f32; 2], 0.0f32);
            for k in 0..self.sources.len() {
                let (noise, chance, pick) = (self.rand() * 2.0 - 1.0, self.rand(), self.rand());
                let (e, r) = self.sources[k].next(noise, chance, pick);
                ears[0] += e[0];
                ears[1] += e[1];
                send += r;
            }
            let wet = self.room.next(send);
            frame[0] += self.level * (ears[0] + wet[0]);
            frame[1] += self.level * (ears[1] + wet[1]);
        }
    }
}

/// A part as the band plays it now: its definition and its level on the way to the mood's.
struct Player {
    part: Part,
    level: f32,
    want: f32,
    /// How many notes it has played this bar (an arpeggio's place).
    n: usize,
}

/// The band: its voices, its room, where it is in the bar, and the mood it plays and the one it is moving to.
pub struct Band {
    voices: Vec<Voice>,
    room: Room,
    rng: u64,
    /// The scale and chords being played (they change on a bar line), and the mood asked for.
    scale: Vec<i32>,
    chords: Vec<i32>,
    next: Option<Mood>,
    players: Vec<Player>,
    /// The songs, the one playing, the bars it has left, and the bars of quiet left before the next.
    songs: Vec<Song>,
    song: usize,
    left: u32,
    quiet: u32,
    tempo: f32,
    night: bool,
    step: usize,
    bar: usize,
    /// Samples until the next step.
    wait: f32,
    /// The lead's motif: two bars of steps, each a step of the scale or a rest.
    motif: [Option<i8>; 2 * STEPS],
}

impl Default for Band {
    fn default() -> Self {
        Self::new(0x2545_f491_4f6c_dd1d)
    }
}

impl Band {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            voices: Vec::with_capacity(VOICES), // alloc: the pool, made once
            room: Room::new(),
            rng: seed | 1,
            scale: Vec::new(),
            chords: Vec::new(),
            next: None,
            players: Vec::new(),
            songs: Vec::new(),
            song: 0,
            left: 0,
            quiet: 0,
            tempo: 72.0,
            night: false,
            step: 0,
            bar: 0,
            wait: 0.0,
            motif: [None; 2 * STEPS],
        }
    }

    /// The songs the band plays, one after another (it starts on one of them by its seed).
    pub fn set_songs(&mut self, songs: Vec<Song>) {
        self.song = if songs.is_empty() { 0 } else { (self.rand() * songs.len() as f32) as usize % songs.len() };
        self.left = songs.get(self.song).map_or(0, |s| s.bars);
        self.songs = songs;
    }

    /// Play toward `mood` (None: fade out and rest), by `night` its night scale. Each of its parts is the player of
    /// that name, at its new level; players it lacks fade out.
    pub fn set(&mut self, mood: Option<&Mood>, night: bool) {
        self.night = night;
        if self.next.as_ref() == mood {
            return;
        }
        self.next = mood.cloned();
        self.cast();
    }

    /// The players for the mood asked for, in the song playing: each of the mood's parts, or the song's part of that
    /// name in its place (at the mood's level).
    fn cast(&mut self) {
        for p in &mut self.players {
            p.want = 0.0;
        }
        let song = self.songs.get(self.song);
        let parts: Vec<Part> = self // alloc: a part a player, on a change of mood or song
            .next
            .iter()
            .flat_map(|m| &m.parts)
            .map(|part| match song.and_then(|s| s.parts.iter().find(|sp| sp.name == part.name)) {
                Some(own) => Part { level: part.level, ..own.clone() },
                None => part.clone(),
            })
            .collect();
        for part in &parts {
            match self.players.iter_mut().find(|p| p.part.name == part.name) {
                Some(p) => {
                    p.want = part.level;
                    p.part = part.clone();
                }
                None => self.players.push(Player { part: part.clone(), level: 0.0, want: part.level, n: 0 }), // alloc: a player a part, on a change of mood
            }
        }
    }

    /// Whether anything sounds or is about to: false once it has faded out and rung out.
    #[must_use]
    pub fn playing(&self) -> bool {
        self.next.is_some() || !self.voices.is_empty() || !self.players.is_empty()
    }

    fn rand(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 40) as f32 / (1u64 << 24) as f32
    }

    /// The pitch of step `degree` of `scale` (past its end: the octaves above; below 0: those below), `octave` up, in
    /// the key `transpose` semitones from A.
    fn pitch(scale: &[i32], degree: i32, octave: i32, transpose: i32) -> f32 {
        let n = scale.len().max(1) as i32;
        let semis = transpose + scale.get(degree.rem_euclid(n) as usize).copied().unwrap_or(0) + 12 * (degree.div_euclid(n) + octave);
        ROOT * 2f32.powf(semis as f32 / 12.0)
    }

    /// A new motif for the lead: mostly rests, notes on the stronger steps, each a step or two from the last.
    fn compose(&mut self) {
        let mut at = 2i32;
        for i in 0..self.motif.len() {
            let chance = if i % 4 == 0 { 0.62 } else if i % 2 == 0 { 0.3 } else { 0.08 };
            self.motif[i] = if self.rand() < chance {
                at = (at + [-2, -1, -1, 1, 1, 2, 3][(self.rand() * 7.0) as usize % 7]).clamp(-2, 9);
                Some(at as i8)
            } else {
                None
            };
        }
        // It ends at home.
        self.motif[2 * STEPS - 4] = Some(0);
    }

    /// One step of the bar: what each player plays on it.
    fn tick(&mut self) {
        if self.step == 0 {
            // The bar line. A song that has run its bars gives way: the band goes quiet for a few bars, then the next
            // song (never the same one twice running) comes in with its own key, chords and parts.
            if self.quiet > 0 {
                self.quiet -= 1;
                if self.quiet == 0 && self.songs.len() > 1 {
                    let step = 1 + (self.rand() * (self.songs.len() - 1) as f32) as usize % (self.songs.len() - 1);
                    self.song = (self.song + step) % self.songs.len();
                    self.left = self.songs[self.song].bars;
                    self.bar = 0;
                    self.compose();
                    self.cast();
                }
            } else if !self.songs.is_empty() && self.next.is_some() {
                self.left = self.left.saturating_sub(1);
                if self.left == 0 && self.songs.len() > 1 {
                    self.quiet = BETWEEN;
                }
            }
            // The scale and chords take over here, every part at once: the mood's own when it has them, else the
            // song's (its bridge every other eight bars).
            if let Some(m) = &self.next {
                let song = self.songs.get(self.song);
                let own = !m.scale.is_empty();
                let (day, night) = match song.filter(|_| !own) {
                    Some(s) => (&s.scale, &s.night),
                    None => (&m.scale, &m.night),
                };
                let scale = if self.night && !night.is_empty() { night } else { day };
                let chords = match song.filter(|_| !own || m.chords.is_empty()) {
                    Some(s) if (self.bar / 8) % 2 == 1 && !s.bridge.is_empty() => &s.bridge,
                    Some(s) => &s.chords,
                    None => &m.chords,
                };
                if &self.scale != scale || &self.chords != chords {
                    self.scale.clone_from(scale);
                    self.chords.clone_from(chords);
                }
            }
            if self.bar % 8 == 0 {
                self.compose();
            } else if self.bar % 2 == 0 {
                // The motif again, one note changed.
                let i = (self.rand() * self.motif.len() as f32) as usize % self.motif.len();
                if let Some(d) = self.motif[i] {
                    self.motif[i] = Some((i32::from(d) + if self.rand() < 0.5 { 1 } else { -1 }).clamp(-2, 9) as i8);
                }
            }
            for p in &mut self.players {
                p.n = 0;
            }
        }
        // Toward the mood asked for: levels and tempo glide (about a bar), never jump. A player faded out leaves.
        let hush = self.quiet > 0;
        for p in &mut self.players {
            p.level += (if hush { 0.0 } else { p.want } - p.level) * 0.09;
        }
        self.players.retain(|p| p.want > 0.0 || p.level > 0.004);
        if let Some(n) = &self.next {
            let pace = self.songs.get(self.song).map_or(1.0, |s| if s.tempo > 0.0 { s.tempo } else { 1.0 });
            self.tempo += (n.tempo * pace - self.tempo) * 0.06;
        }
        if self.scale.is_empty() || self.chords.is_empty() {
            return;
        }
        let (step, bar) = (self.step, self.bar);
        let root = self.chords[bar % self.chords.len()];
        let chord = [root, root + 2, root + 4];
        let beat = 60.0 / self.tempo.max(30.0);
        let len = self.scale.len() as i32;
        let key = self.songs.get(self.song).map_or(0, |s| s.transpose);
        for k in 0..self.players.len() {
            let (role, on, chance) = {
                let p = &self.players[k];
                (p.part.role, p.part.steps.contains(&(step as u8)), p.part.chance)
            };
            let motif = if role == Role::Motif { self.motif[(bar % 2) * STEPS + step] } else { None };
            if !(on || motif.is_some()) || (role == Role::Motif && motif.is_none()) {
                continue;
            }
            if chance < 1.0 && self.rand() >= chance {
                continue;
            }
            let pick = (self.rand() * 3.0) as usize % 3;
            let p = &mut self.players[k];
            let (part, level) = (&p.part, p.level);
            if level < 0.003 {
                continue;
            }
            let accent = if step % 4 == 0 { 1.0 } else { 0.7 };
            let hold = (part.beats * beat * SR) as u32;
            let mut notes: [Option<(f32, f32)>; 3] = [None; 3];
            match role {
                Role::Chord => {
                    for (j, d) in chord.iter().enumerate() {
                        notes[j] = Some((Self::pitch(&self.scale, *d, part.octave, key), part.pan + [-0.5, 0.0, 0.5][j]));
                    }
                }
                Role::Root => notes[0] = Some((Self::pitch(&self.scale, root, part.octave, key), part.pan)),
                Role::Arp => {
                    let d = chord[p.n % 3] + len * ((p.n / 3) % 2) as i32;
                    notes[0] = Some((Self::pitch(&self.scale, d, part.octave, key), if p.n % 2 == 0 { -0.35 } else { 0.35 }));
                }
                Role::Motif => notes[0] = motif.map(|d| (Self::pitch(&self.scale, root + i32::from(d), part.octave, key), part.pan)),
                Role::Sparkle => notes[0] = Some((Self::pitch(&self.scale, chord[pick], part.octave, key), part.pan + pick as f32 * 0.5 - 0.5)),
                Role::Hit => notes[0] = Some((part.hz, part.pan)),
            }
            p.n += 1;
            let inst = part.instrument.clone();
            let amp = level * if matches!(role, Role::Chord) { 1.0 } else { accent };
            for (freq, pan) in notes.into_iter().flatten() {
                if self.voices.len() >= VOICES {
                    self.voices.remove(0);
                }
                self.voices.push(Voice { inst: inst.clone(), freq, amp, pan: pan.clamp(-1.0, 1.0), t: 0, hold, phase: 0.0, last: 0.0 });
            }
        }
    }

    /// Add the band's next `out.len()` frames to `out`.
    pub fn render(&mut self, out: &mut [[f32; 2]]) {
        if !self.playing() {
            return;
        }
        for frame in out {
            if self.wait <= 0.0 {
                self.tick();
                self.wait += 60.0 / self.tempo.max(30.0) / 4.0 * SR;
                self.step += 1;
                if self.step == STEPS {
                    self.step = 0;
                    self.bar += 1;
                }
            }
            self.wait -= 1.0;
            let noise = self.rand() * 2.0 - 1.0;
            let (mut dry, mut send) = ([0.0f32; 2], 0.0f32);
            self.voices.retain_mut(|v| {
                let (s, ended) = v.next(noise);
                let s = s * v.amp;
                dry[0] += s * (0.5 - 0.4 * v.pan);
                dry[1] += s * (0.5 + 0.4 * v.pan);
                send += s * v.inst.room;
                !ended
            });
            let wet = self.room.next(send);
            frame[0] += dry[0] + wet[0];
            frame[1] += dry[1] + wet[1];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(name: &str, role: Role, steps: &[u8], level: f32, hold: bool) -> Part {
        let instrument = Arc::new(Instrument { partials: vec![(1.0, 1.0), (2.0, 0.4)], attack: if hold { 1.0 } else { 0.003 }, decay: if hold { 1.4 } else { 3.0 }, hold, damp: 1.0, room: 0.3, ..Instrument::default() });
        Part { name: name.to_owned(), instrument, role, steps: steps.to_vec(), octave: 1, level, pan: 0.0, chance: 1.0, beats: 4.0, hz: 60.0 }
    }

    fn mood(tempo: f32, parts: Vec<Part>) -> Mood {
        Mood { tempo, scale: vec![0, 2, 4, 7, 9], night: vec![0, 2, 3, 7, 8], chords: vec![0, 3, 2, 4], parts }
    }

    fn seconds(b: &mut Band, s: f32) -> (f32, f32) {
        let mut out = vec![[0.0f32; 2]; (s * SR) as usize];
        b.render(&mut out);
        let peak = out.iter().flatten().fold(0.0f32, |p, x| p.max(x.abs()));
        let rms = (out.iter().flatten().map(|x| x * x).sum::<f32>() / (out.len() * 2) as f32).sqrt();
        (peak, rms)
    }

    /// The band plays a mood under full scale, a fuller mood is louder, a change of mood glides (no step in level
    /// within its first second), a part the new mood lacks leaves, and with no mood it fades out, rings out and rests.
    #[test]
    fn the_band_follows_its_mood_and_rests() {
        let calm = mood(66.0, vec![part("pad", Role::Chord, &[0], 0.06, true), part("lead", Role::Motif, &[], 0.06, false)]);
        let all: Vec<u8> = (0..16).collect();
        let battle = mood(132.0, vec![part("lead", Role::Motif, &[], 0.12, false), part("arp", Role::Arp, &all, 0.12, false), part("kick", Role::Hit, &[0, 8], 0.3, false)]);
        let mut b = Band::new(7);
        assert!(!b.playing());
        b.set(Some(&calm), false);
        let (peak, quiet) = seconds(&mut b, 12.0);
        assert!(peak > 0.01 && peak < 0.9, "calm peaks at {peak}");
        b.set(Some(&battle), false);
        let (_, first) = seconds(&mut b, 1.0);
        assert!(first < quiet * 6.0, "the change jumped: {quiet} to {first} within a second");
        let (peak, loud) = seconds(&mut b, 12.0);
        assert!(peak < 0.98 && loud > quiet * 1.5, "battle peaks at {peak}, rms {loud} over calm's {quiet}");
        assert!(b.players.iter().all(|p| p.part.name != "pad"), "the pad, which battle lacks, never left");
        b.set(None, false);
        seconds(&mut b, 30.0);
        assert!(!b.playing(), "it never came to rest");
        let (peak, _) = seconds(&mut b, 1.0);
        assert!(peak == 0.0, "at rest it still sounds: {peak}");
    }

    /// With songs, the band plays one for its bars, goes quiet between, and comes in again on another, in that song's
    /// key and with its own part in the mood's place.
    #[test]
    fn the_band_moves_from_song_to_song() {
        let song = |name: &str, transpose: i32, lead: Option<Part>| Song { name: name.to_owned(), transpose, tempo: 1.0, bars: 2, scale: vec![0, 2, 4, 7, 9], night: vec![], chords: vec![0, 3], bridge: vec![], parts: lead.into_iter().collect() };
        let own = part("lead", Role::Root, &[0, 4, 8, 12], 1.0, true);
        let mut m = mood(240.0, vec![part("lead", Role::Root, &[0, 8], 0.1, false)]);
        (m.scale, m.night, m.chords) = (Vec::new(), Vec::new(), Vec::new());
        let mut b = Band::new(3);
        b.set_songs(vec![song("a", 0, None), song("b", 5, Some(own))]);
        b.set(Some(&m), false);
        let first = b.song;
        let mut heard = vec![first];
        let mut hushed = false;
        for _ in 0..40 {
            seconds(&mut b, 1.0);
            hushed |= b.quiet > 0;
            if heard.last() != Some(&b.song) {
                heard.push(b.song);
            }
        }
        assert!(hushed, "no quiet between songs");
        assert!(heard.len() >= 3 && heard.windows(2).all(|w| w[0] != w[1]), "the songs it played: {heard:?}");
        // In song b the lead is b's own part (it holds its notes), at the mood's level.
        while b.song != 1 || b.quiet > 0 {
            seconds(&mut b, 0.5);
        }
        let lead = b.players.iter().find(|p| p.part.name == "lead").expect("a lead");
        assert!(lead.part.instrument.hold && (lead.want - 0.1).abs() < 1e-6, "song b's lead is not its own part at the mood's level");
    }

    /// Rain comes in, stays under its level, and goes; a source at the left is louder and earlier in the left ear, one
    /// overhead the same in both.
    #[test]
    fn rain_falls_where_it_is_placed() {
        let on = |azimuth: f32, elevation: f32| RainSource { azimuth, elevation, distance: 1.5, gain: 1.0, hiss: 0.5, rate: 20.0, low: 800.0, high: 3000.0, ring: 60.0, bend: 0.0 };
        let ears = |src: RainSource| {
            let mut r = Rain::new(&[src]);
            r.set(0.1);
            let mut out = vec![[0.0f32; 2]; (8.0 * SR) as usize];
            r.render(&mut out);
            let tail = &out[out.len() - 2 * SR as usize..];
            let rms = |side: usize| (tail.iter().map(|f| f[side] * f[side]).sum::<f32>() / tail.len() as f32).sqrt();
            (rms(0), rms(1), tail.iter().flatten().fold(0.0f32, |p, x| p.max(x.abs())))
        };
        let (l, r, peak) = ears(on(-80.0, 0.0));
        assert!(l > r * 1.4 && r > 0.0005 && peak < 0.3, "at the left: {l} against {r}, peak {peak}");
        let (l, r, _) = ears(on(0.0, 90.0));
        assert!((l / r - 1.0).abs() < 0.15, "overhead: {l} against {r}");
        let mut r = Rain::new(&[on(0.0, 0.0)]);
        assert!(!r.playing());
        r.set(0.1);
        r.render(&mut vec![[0.0f32; 2]; SR as usize]);
        r.set(0.0);
        r.render(&mut vec![[0.0f32; 2]; (20.0 * SR) as usize]);
        assert!(!r.playing());
    }
}
