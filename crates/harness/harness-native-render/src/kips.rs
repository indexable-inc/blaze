//! The Kips a painter draws (a `Kip` node, harness-native-kip): each one's character, motion and act kept from frame to
//! frame by its node, stepped on the frame clock while it is painted, and set into the `Draw` as a mascot quad (kip.rs
//! renders it). After den-native's ui/kip.rs:
//!
//! - He plays his moods when he first comes into view, or with `Kip.settle` stands where they end (a reply read back from
//!   history, so opening an old chat does not set every Kip in it waving). When the pointer comes onto him after they have
//!   ended, he plays them again. Another `mood` lays his act out again and plays it.
//! - He moves only while he is painted: a node that was not painted the frame before (scrolled away, hidden, its pane
//!   closed) stops his clock, and one not painted for a while is let go.
//! - He looks toward the pointer while it is in the window.
//! - His look and motion are the scene's `ui.kip` (harness-native-ui-prims kip.flecs), read once a frame.
//!
//! The host asks [`Kips::wake`] after each frame: every frame while one moves fast (a hop, a wave, a crossfade), a calm
//! rate while one only breathes or springs, else how long until the next blink or fidget, when it calls [`Kips::rest`]
//! with the time that went by.
use std::collections::HashMap;

use harness_native_flecs::prelude::*;
use harness_native_kip::act::{self, Act};
use harness_native_kip::{Character, Clip, Ink, Look, Mascot, Mood, Motion, Pair, Setup, Tuning};
use harness_native_ui_input_api::PointerState;
use harness_native_ui_look_api::{Fill, MascotClip, MascotInk, MascotLook, MascotMotion};
use harness_native_ui_text_api::{Kip, KipStand};

use crate::draw::Draw;

/// The scene's one look and motion of Kip.
pub const LOOK: &str = "ui::kip";

/// Frames a Kip may go unpainted before he is let go.
const FORGET: u64 = 600;

/// One Kip on screen.
struct One {
    kip: Mascot,
    act: Act,
    /// The `mood` he was asked for: another lays his act out again.
    mood: String,
    /// The pointer is on him.
    over: bool,
    /// The frame he was last painted, and the clock's time then.
    seen: u64,
    time: f32,
}

/// What the host should do about the Kips in view after a frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Wake {
    /// None in view.
    None,
    /// One moves fast (a hop, a wave, a crossfade, a walk): draw every frame.
    Lively,
    /// One breathes, blinks, glances or springs: draw at a calm rate. The name is what moves.
    Calm(&'static str),
    /// All rest: wake in this many seconds for a blink or a fidget, and tell [`Kips::rest`] how long it was.
    Rest(f64),
}

/// The Kips of one painter.
#[derive(Default)]
pub struct Kips {
    ones: HashMap<u64, One>,
    /// The painter's frame (counted by [`Kips::end_frame`]) and the setup read in it.
    frame: u64,
    setup: Option<(u64, Option<Setup>)>,
}

/// `ui.kip`'s look and motion, its colours and inks from its children and its moods' clips; Err names what is missing or
/// out of range, None when the scene has no `ui.kip` (a host that never loaded kip.flecs).
pub fn setup(w: WorldRef<'_>) -> Result<Option<Setup>, String> {
    let Some(m) = w.try_lookup(LOOK) else { return Ok(None) };
    let (Some(look), Some(mm)) = (m.try_cloned::<&MascotLook>(), m.try_cloned::<&MascotMotion>()) else {
        return Err(format!("{LOOK} needs MascotLook and MascotMotion (kip.flecs)"));
    };
    let rgb = |c: harness_native_ui_look_api::Rgba| [c.r, c.g, c.b];
    let pair = |name: &str| -> Result<(Pair, [f32; 2]), String> {
        let f = w
            .try_lookup(&format!("{LOOK}::mascot_{name}"))
            .and_then(|e| e.try_cloned::<&Fill>())
            .ok_or(format!("{LOOK} needs a `mascot_{name}` child with Fill (kip.flecs)"))?;
        Ok((Pair { dark: rgb(f.dark), light: rgb(f.light) }, [f.dark.a, f.light.a]))
    };
    let mut inks = Vec::new();
    if let Some(e) = w.try_lookup(&format!("{LOOK}::inks")) {
        e.each_child(|c| {
            if let (Some(f), Some(name)) = (c.try_cloned::<&Fill>(), c.name().strip_prefix("mascot_ink_")) {
                let glow = c.try_cloned::<&MascotInk>().map_or(0.0, |k| k.glow);
                inks.push(Ink { name: name.to_owned(), color: Pair { dark: rgb(f.dark), light: rgb(f.light) }, glow });
            }
        });
    }
    inks.sort_by(|a, b| a.name.cmp(&b.name));
    let mut clips = [Clip { blend: 0.0, speed: 1.0, amount: 1.0, length: 0.0 }; 7];
    for (i, mood) in Mood::ALL.into_iter().enumerate() {
        let c = w
            .try_lookup(&format!("{LOOK}::{}", mood.name()))
            .and_then(|e| e.try_cloned::<&MascotClip>())
            .ok_or(format!("{LOOK} needs a `{}` child with MascotClip (kip.flecs)", mood.name()))?;
        if c.blend < 0.0 || c.speed <= 0.0 || (mood.one_shot() && c.length <= 0.0) {
            return Err(format!(
                "{LOOK}.{}: blend is seconds from 0, speed above 0, and a one-shot lasts above 0 s: {c:?}",
                mood.name()
            ));
        }
        clips[i] = Clip { blend: c.blend, speed: c.speed, amount: c.amount, length: c.length };
    }
    if mm.breath_hz <= 0.0
        || mm.blink_min <= 0.0
        || mm.blink_max < mm.blink_min
        || mm.blink_len <= 0.0
        || mm.look_omega <= 0.0
        || mm.turn_omega <= 0.0
        || mm.stride <= 0.0
        || mm.cadence <= 0.0
        || mm.accel <= 0.0
        || mm.hop_time <= 0.0
        || mm.fidget_every <= 0.0
    {
        return Err(format!("{LOOK}: MascotMotion's rates, times and sizes are above 0, blink_max at least blink_min: {mm:?}"));
    }
    let motion = Motion {
        breath_hz: mm.breath_hz,
        breath: mm.breath,
        blink_min: mm.blink_min,
        blink_max: mm.blink_max,
        blink_len: mm.blink_len,
        look_omega: mm.look_omega,
        look_yaw: mm.look_yaw.to_radians(),
        look_pitch: mm.look_pitch.to_radians(),
        turn_omega: mm.turn_omega,
        antenna_hz: mm.antenna_hz,
        antenna_damping: mm.antenna_damping,
        antenna_gain: mm.antenna_gain,
        arm_hz: mm.arm_hz,
        arm_damping: mm.arm_damping,
        arm_gain: mm.arm_gain,
        stride: mm.stride,
        cadence: mm.cadence,
        accel: mm.accel,
        hop_height: mm.hop_height,
        hop_time: mm.hop_time,
        anticipation: mm.anticipation,
        sleep_after: mm.sleep_after,
        awake: mm.awake,
        fidget_every: mm.fidget_every,
    };
    let (key, _) = pair("key_light")?;
    let (sky, _) = pair("sky")?;
    let (ground, _) = pair("ground")?;
    let (rim, _) = pair("rim")?;
    let (shadow, shadow_alpha) = pair("shadow")?;
    let look = Look {
        pitch: look.pitch.to_radians(),
        yaw: look.yaw.to_radians(),
        key_dir: [look.key_x, look.key_y, look.key_z],
        key,
        key_strength: look.key,
        sky,
        ground,
        fill: look.fill,
        rim_dir: [look.rim_x, look.rim_y, look.rim_z],
        rim,
        rim_strength: look.rim,
        rim_power: look.rim_power,
        ao: look.ao,
        seams: look.seams,
        shadow,
        shadow_alpha,
        inks,
        fade: look.fade,
    };
    Ok(Some(Setup { look, tuning: Tuning { motion, clips } }))
}

/// The height (points) a `Kip` asks for, kept to the heights he may be drawn at.
#[must_use]
pub fn tall_of(k: &Kip) -> f32 {
    let s = if k.size.is_finite() && k.size > 0.0 { k.size } else { 64.0 };
    s.clamp(harness_native_kip::SHORTEST, harness_native_kip::TALLEST)
}

/// The box a `Kip` node measures (its `Intrinsic`): none for one a `KipStand` places.
#[must_use]
pub fn measure(e: EntityView<'_>, k: &Kip) -> Option<[f32; 2]> {
    (!e.has(KipStand::id())).then(|| harness_native_kip::gauge(&Character::kip(), tall_of(k), k.centre))
}

/// What his picture is kept under: his node's id, apart from anything else's.
fn picture_id(entity: u64) -> u64 {
    entity | 1 << 62
}

impl Kips {
    fn setup(&mut self, w: WorldRef<'_>) -> Result<Option<Setup>, String> {
        if let Some((f, s)) = &self.setup
            && *f == self.frame
        {
            return Ok(s.clone());
        }
        let s = setup(w)?;
        self.setup = Some((self.frame, s.clone()));
        Ok(s)
    }

    /// Kip node `e` (its `Kip` `k`) at `rect` (posed) over `laid`, at `alpha`, the clock at `time`: stepped and drawn.
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &mut self,
        d: &mut Draw,
        e: EntityView<'_>,
        k: &Kip,
        rect: [f32; 4],
        laid: [f32; 4],
        alpha: f32,
        dark: bool,
        sc: f32,
        time: f32,
    ) -> Result<(), String> {
        let w = e.world();
        let Some(setup) = self.setup(w)? else {
            return Err(format!("{}: a Kip, but the scene has no {LOOK} (kip.flecs)", e.path()));
        };
        let ch = Character::kip();
        let id = *e.id();
        let frame = self.frame;
        let mood = k.mood.as_str();
        let one = self.ones.entry(id).or_insert_with(|| One {
            kip: Mascot::new(setup.tuning, ch.rig.height, [0.0, 0.0], id),
            act: Act::new(act::moods(mood)),
            mood: mood.to_owned(),
            over: false,
            seen: 0,
            time,
        });
        if one.mood != mood {
            one.act = Act::new(act::moods(mood));
            one.mood = mood.to_owned();
        }
        if one.kip.tuning != setup.tuning {
            one.kip.tuning = setup.tuning;
        }
        // Under Reduce Motion (the window keeps it in step with AppKit's) he rests still, blinking only.
        one.kip.reduce_motion = harness_native_api::reduced(w);
        // His clock runs only while he is painted frame after frame: off screen it stopped.
        let dt = if one.seen + 1 == frame { (time - one.time).clamp(0.0, 0.25) } else { 0.0 };
        one.seen = frame;
        one.time = time;
        let tall = tall_of(k) * if laid[2] > 0.0 { rect[2] / laid[2] } else { 1.0 };
        let stand = match e.try_cloned::<&KipStand>() {
            Some(s) => [rect[0] + s.x * rect[2] + s.dx, rect[1] + s.y * rect[3] + s.dy],
            None => harness_native_kip::feet(&ch, rect, tall, k.centre),
        };
        one.kip.set_height(tall);
        if (one.kip.at()[0] - stand[0]).abs() > 0.01 || (one.kip.at()[1] - stand[1]).abs() > 0.01 {
            one.kip.place(stand);
        }
        if !one.act.started() {
            if k.settle { one.act.settle(&mut one.kip) } else { one.act.play(&mut one.kip) }
        }
        // The pointer on him: his box, or where he stands on another's edge (his own height about his feet).
        let p = w.try_get::<&PointerState>(|p| [p.x, p.y]).filter(|p| p[0] >= 0.0 && p[1] >= 0.0);
        let cell = tall / ch.rig.height.max(1.0);
        let body = [stand[0] - (ch.rig.half_width + 1.0) * cell, stand[1] - tall, (2.0 * ch.rig.half_width + 2.0) * cell, tall];
        let over = p.is_some_and(|p| p[0] >= body[0] && p[0] < body[0] + body[2] && p[1] >= body[1] && p[1] < body[1] + body[3]);
        if over && !one.over && one.act.done(&one.kip) {
            one.act.play(&mut one.kip);
        }
        one.over = over;
        one.kip.look_at(p);
        one.act.tick(&mut one.kip);
        one.kip.step(dt);
        if alpha < 0.004 {
            return Ok(());
        }
        let q = harness_native_kip::quad(
            picture_id(id),
            &ch,
            &one.kip.pose(),
            &setup.look,
            one.kip.at(),
            tall,
            alpha,
            f32::from(u8::from(dark)),
            sc,
        );
        d.kip(q);
        Ok(())
    }

    /// After a frame: what the Kips painted in it want next, and the frame counted on.
    pub fn end_frame(&mut self) -> Wake {
        let frame = self.frame;
        self.frame += 1;
        self.ones.retain(|_, o| frame - o.seen.min(frame) < FORGET);
        let shown = || self.ones.values().filter(|o| o.seen == frame);
        if shown().next().is_none() {
            return Wake::None;
        }
        if shown().any(|o| o.kip.lively()) {
            return Wake::Lively;
        }
        if let Some(why) = shown().find_map(|o| o.kip.moves()) {
            return Wake::Calm(why);
        }
        Wake::Rest(shown().map(|o| o.kip.rest_for()).fold(f64::INFINITY, f64::min))
    }

    /// While nothing was drawn, `secs` went by: the clocks of the Kips last in view move on. Whether one is due to blink or
    /// fidget now.
    pub fn rest(&mut self, secs: f64) -> bool {
        let last = self.frame.saturating_sub(1);
        let mut due = false;
        for o in self.ones.values_mut().filter(|o| o.seen == last) {
            o.kip.resume(secs);
            due |= o.kip.rest_for() <= 0.0 || o.kip.animating();
        }
        due
    }

    /// For a shot or a test: each Kip by its node, with the mood he shows now and the act he was asked for.
    #[must_use]
    pub fn shown(&self) -> Vec<(u64, Mood, Vec<Mood>)> {
        let mut out: Vec<_> = self.ones.iter().map(|(id, o)| (*id, o.kip.mood(), o.act.steps().to_vec())).collect();
        out.sort_by_key(|k| k.0);
        out
    }
}
