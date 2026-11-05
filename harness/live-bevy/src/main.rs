//! A bevy app without a window, for harness/live_latency.py: two teams of ships, the rocks they
//! shoot and the shots, in a field that wraps around, on bevy's ECS. Most of its systems are
//! generic (one instance per marker or team they are added for), as bevy code tends to be. bevy's
//! schedule runner steps it about once a millisecond.
//!
//! It takes edits while it runs: blaze-live applies each patch the moment it lands, and the
//! next frame runs the new systems. `answer()` is what live_latency.py's socket and burst modes
//! edit; the frame prints it when it changes (`answer N at_us=`), and each patch's outcome.

use std::io::Write as _;
use std::marker::PhantomData;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;

/// Half the width of the field: a body that leaves one side comes back on the other.
const HALF: f32 = 400.0;
/// Rocks at the start.
const ROCKS: usize = 48;
/// Ships of each team.
const SHIPS: usize = 6;
/// How long a shot flies, in seconds.
const SHOT_LIFE: f32 = 1.5;
/// How fast a shot leaves its ship.
const SHOT_SPEED: f32 = 220.0;
/// The fastest a ship goes.
const TOP_SPEED: f32 = 90.0;

#[inline(never)]
fn answer() -> u64 {
    1
}

// ---- Components and resources. -----------------------------------------------------------

#[derive(Component, Clone, Copy)]
pub struct Velocity(pub Vec3);

/// How fast a body turns, in radians a second.
#[derive(Component, Clone, Copy)]
pub struct Spin(pub f32);

#[derive(Component)]
pub struct Ship {
    pub fuel: f32,
    pub cooldown: f32,
    pub kills: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Size {
    Small,
    Medium,
    Large,
}

#[derive(Component)]
pub struct Rock {
    pub size: Size,
    pub hp: u32,
}

#[derive(Component)]
pub struct Shot {
    pub life: f32,
    pub team: u8,
}

/// The two teams. A team is a type: the systems that steer and fire are added once for each.
pub trait Team: Component + Default {
    const ID: u8;
    const NAME: &'static str;
    /// Where its ships start, and the way they lean.
    const HOME: Vec3;
}

#[derive(Component, Default)]
pub struct Red;

#[derive(Component, Default)]
pub struct Blue;

impl Team for Red {
    const ID: u8 = 0;
    const NAME: &'static str = "red";
    const HOME: Vec3 = Vec3::new(-HALF / 2.0, 0.0, 0.0);
}

impl Team for Blue {
    const ID: u8 = 1;
    const NAME: &'static str = "blue";
    const HOME: Vec3 = Vec3::new(HALF / 2.0, 0.0, 0.0);
}

/// What a kind of target is worth.
pub trait Scored {
    const POINTS: u64;
}

pub struct Pebble;
pub struct Boulder;

impl Scored for Pebble {
    const POINTS: u64 = 10;
}

impl Scored for Boulder {
    const POINTS: u64 = 25;
}

/// How many entities have `T`, counted each frame.
#[derive(Resource)]
pub struct Census<T: Component> {
    pub count: usize,
    pub peak: usize,
    marker: PhantomData<T>,
}

impl<T: Component> Default for Census<T> {
    fn default() -> Self {
        Census { count: 0, peak: 0, marker: PhantomData }
    }
}

/// The score of each team, and what the field has seen.
#[derive(Resource, Default)]
pub struct Tally {
    pub score: [u64; 2],
    pub hits: u64,
    pub splits: u64,
    pub frames: u64,
    pub reseeded: u64,
}

/// The random numbers of the simulation: the same run every time.
#[derive(Resource)]
pub struct Dice(pub u64);

impl Default for Dice {
    fn default() -> Self {
        Dice(0x2545_f491_4f6c_dd1d)
    }
}

/// A shot of `team` hit `target`.
#[derive(Message, Clone, Copy)]
pub struct Hit {
    pub target: Entity,
    pub shot: Entity,
    pub team: u8,
}

/// Something of `A` came within reach of something of `B`.
#[derive(Message)]
pub struct Near<A: Component, B: Component> {
    pub a: Entity,
    pub b: Entity,
    pub distance: f32,
    marker: PhantomData<(A, B)>,
}

// ---- Plain functions. ----------------------------------------------------------------------

/// The next number of a xorshift generator.
#[inline(never)]
fn next(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// A number in [0, 1).
fn unit(state: &mut u64) -> f32 {
    (next(state) >> 40) as f32 / (1u64 << 24) as f32
}

/// A number in [-1, 1).
fn signed(state: &mut u64) -> f32 {
    unit(state) * 2.0 - 1.0
}

/// `x` brought back into the field.
#[inline(never)]
fn wrap_coord(x: f32) -> f32 {
    if x > HALF {
        x - 2.0 * HALF
    } else if x < -HALF {
        x + 2.0 * HALF
    } else {
        x
    }
}

/// The shortest way from `a` to `b` in a field that wraps.
fn toward(a: Vec3, b: Vec3) -> Vec3 {
    Vec3::new(wrap_coord(b.x - a.x), wrap_coord(b.y - a.y), 0.0)
}

/// How far a rock of this size reaches.
fn radius(size: Size) -> f32 {
    match size {
        Size::Small => 8.0,
        Size::Medium => 16.0,
        Size::Large => 28.0,
    }
}

/// How many hits a rock of this size takes.
fn toughness(size: Size) -> u32 {
    match size {
        Size::Small => 1,
        Size::Medium => 2,
        Size::Large => 4,
    }
}

/// What a rock breaks into: the size of the pieces and how many.
fn split(size: Size) -> Option<(Size, usize)> {
    match size {
        Size::Large => Some((Size::Medium, 2)),
        Size::Medium => Some((Size::Small, 3)),
        Size::Small => None,
    }
}

/// `v` no faster than `top`.
fn clamp_speed(v: Vec3, top: f32) -> Vec3 {
    let speed = v.length();
    if speed > top { v * (top / speed) } else { v }
}

/// The direction of angle `a`, in the plane.
fn heading(a: f32) -> Vec3 {
    Vec3::new(a.cos(), a.sin(), 0.0)
}

/// The angle of `v`, in the plane.
fn angle_of(v: Vec3) -> f32 {
    v.y.atan2(v.x)
}

/// The points of hitting `hits` times a target worth `K`.
#[inline(never)]
fn score_of<K: Scored>(hits: u64) -> u64 {
    K::POINTS * hits
}

/// The smallest of `values` by `key`, and its index.
fn least_by<T, F: Fn(&T) -> f32>(values: &[T], key: F) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for (i, value) in values.iter().enumerate() {
        let k = key(value);
        if best.is_none_or(|(_, b)| k < b) {
            best = Some((i, k));
        }
    }
    best
}

/// The mean of `points`, or the origin.
fn centre<'a, I: IntoIterator<Item = &'a Vec3>>(points: I) -> Vec3 {
    let mut sum = Vec3::ZERO;
    let mut n = 0;
    for p in points {
        sum += *p;
        n += 1;
    }
    if n == 0 { Vec3::ZERO } else { sum / n as f32 }
}

// ---- Spawning. -----------------------------------------------------------------------------

fn spawn_rock(commands: &mut Commands, dice: &mut Dice, at: Vec3, size: Size) {
    let v = Vec3::new(signed(&mut dice.0), signed(&mut dice.0), 0.0) * 30.0;
    commands.spawn((
        Transform::from_translation(at),
        Velocity(v),
        Spin(signed(&mut dice.0) * 2.0),
        Rock { size, hp: toughness(size) },
    ));
}

fn spawn_rocks(mut commands: Commands, mut dice: ResMut<Dice>) {
    for i in 0..ROCKS {
        let at = Vec3::new(signed(&mut dice.0) * HALF, signed(&mut dice.0) * HALF, 0.0);
        let size = match i % 3 {
            0 => Size::Large,
            1 => Size::Medium,
            _ => Size::Small,
        };
        spawn_rock(&mut commands, &mut dice, at, size);
    }
}

fn spawn_team<T: Team>(mut commands: Commands, mut dice: ResMut<Dice>) {
    for i in 0..SHIPS {
        let lean = i as f32 / SHIPS as f32 * std::f32::consts::TAU;
        let at = T::HOME + heading(lean) * 40.0;
        commands.spawn((
            Transform::from_translation(at),
            Velocity(heading(lean + signed(&mut dice.0)) * 20.0),
            Ship { fuel: 100.0, cooldown: 0.0, kills: 0 },
            T::default(),
        ));
    }
}

// ---- Systems that move things. -----------------------------------------------------------

/// Moves everything with `A` by its velocity.
fn drift<A: Component>(time: Res<Time>, mut bodies: Query<(&mut Transform, &Velocity), With<A>>) {
    let dt = time.delta_secs().min(0.05);
    for (mut transform, velocity) in &mut bodies {
        transform.translation += velocity.0 * dt;
    }
}

/// Turns everything with `A` that spins.
fn turn<A: Component>(time: Res<Time>, mut bodies: Query<(&mut Transform, &Spin), With<A>>) {
    let dt = time.delta_secs().min(0.05);
    for (mut transform, spin) in &mut bodies {
        transform.rotate_z(spin.0 * dt);
    }
}

/// Brings everything with `A` back into the field.
fn wrap<A: Component>(mut bodies: Query<&mut Transform, With<A>>) {
    for mut transform in &mut bodies {
        let t = &mut transform.translation;
        t.x = wrap_coord(t.x);
        t.y = wrap_coord(t.y);
    }
}

/// Counts what has `T`.
fn census<T: Component>(things: Query<(), With<T>>, mut census: ResMut<Census<T>>) {
    census.count = things.iter().count();
    census.peak = census.peak.max(census.count);
}

/// The ships of team `T`: each turns toward the nearest rock, leans away from its own team's
/// crowd, and keeps under its top speed while it has fuel.
fn steer<T: Team>(
    time: Res<Time>,
    mut ships: Query<(&Transform, &mut Velocity, &mut Ship), With<T>>,
    rocks: Query<&Transform, (With<Rock>, Without<Ship>)>,
) {
    let dt = time.delta_secs().min(0.05);
    let targets: Vec<Vec3> = rocks.iter().map(|it| it.translation).collect();
    let crowd = centre(ships.iter().map(|(t, _, _)| &t.translation).collect::<Vec<_>>());
    for (transform, mut velocity, mut ship) in &mut ships {
        let here = transform.translation;
        let nearest = least_by(&targets, |it| toward(here, *it).length_squared());
        let mut push = Vec3::ZERO;
        if let Some((i, _)) = nearest {
            push += toward(here, targets[i]).normalize_or_zero() * 40.0;
        }
        let away = toward(crowd, here);
        if away.length() < 60.0 {
            push += away.normalize_or_zero() * 15.0;
        }
        if ship.fuel > 0.0 {
            velocity.0 += push * dt;
            ship.fuel = (ship.fuel - push.length() * dt * 0.01).max(0.0);
        } else {
            velocity.0 *= 0.995;
        }
        let speed = velocity.0.length();
        if speed < 5.0 {
            velocity.0 += heading(angle_of(push) + 0.5) * 5.0;
        }
        velocity.0 = clamp_speed(velocity.0, TOP_SPEED);
        ship.cooldown = (ship.cooldown - dt).max(0.0);
    }
}

/// The ships of team `T` that are ready fire at what they head toward.
fn fire<T: Team>(mut commands: Commands, mut ships: Query<(&Transform, &Velocity, &mut Ship), With<T>>) {
    for (transform, velocity, mut ship) in &mut ships {
        if ship.cooldown > 0.0 || velocity.0.length() < 1.0 {
            continue;
        }
        ship.cooldown = 0.4;
        let way = velocity.0.normalize_or_zero();
        commands.spawn((
            Transform::from_translation(transform.translation + way * 6.0),
            Velocity(way * SHOT_SPEED + velocity.0),
            Shot { life: SHOT_LIFE, team: T::ID },
        ));
    }
}

/// Shots age and are gone when their time is up.
fn age_shots(time: Res<Time>, mut commands: Commands, mut shots: Query<(Entity, &mut Shot)>) {
    let dt = time.delta_secs().min(0.05);
    for (entity, mut shot) in &mut shots {
        shot.life -= dt;
        if shot.life <= 0.0 {
            commands.entity(entity).try_despawn();
        }
    }
}

// ---- Collisions. ---------------------------------------------------------------------------

/// Whatever of `A` comes within `reach` of something of `B`, each pair once a frame.
fn near<A: Component, B: Component>(
    a: Query<(Entity, &Transform), With<A>>,
    b: Query<(Entity, &Transform), With<B>>,
    mut out: MessageWriter<Near<A, B>>,
) {
    const REACH: f32 = 30.0;
    for (ea, ta) in &a {
        for (eb, tb) in &b {
            let distance = toward(ta.translation, tb.translation).length();
            if distance < REACH {
                out.write(Near { a: ea, b: eb, distance, marker: PhantomData });
            }
        }
    }
}

/// A shot that is near a rock and inside it hits it.
fn shots_hit(
    mut near: MessageReader<Near<Shot, Rock>>,
    shots: Query<&Shot>,
    rocks: Query<&Rock>,
    mut hits: MessageWriter<Hit>,
) {
    let mut spent: Vec<Entity> = Vec::new();
    for it in near.read() {
        if spent.contains(&it.a) {
            continue;
        }
        let (Ok(shot), Ok(rock)) = (shots.get(it.a), rocks.get(it.b)) else { continue };
        if it.distance < radius(rock.size) {
            spent.push(it.a);
            hits.write(Hit { target: it.b, shot: it.a, team: shot.team });
        }
    }
}

/// A rock that is hit loses a point; one with none left breaks into smaller ones, and the team
/// that broke it scores.
fn break_rocks(
    mut commands: Commands,
    mut hits: MessageReader<Hit>,
    mut rocks: Query<(&Transform, &mut Rock)>,
    mut tally: ResMut<Tally>,
    mut dice: ResMut<Dice>,
) {
    let mut broken: Vec<Entity> = Vec::new();
    for hit in hits.read() {
        commands.entity(hit.shot).try_despawn();
        if broken.contains(&hit.target) {
            continue;
        }
        let Ok((transform, mut rock)) = rocks.get_mut(hit.target) else { continue };
        tally.hits += 1;
        rock.hp = rock.hp.saturating_sub(1);
        if rock.hp > 0 {
            continue;
        }
        broken.push(hit.target);
        let points = match rock.size {
            Size::Small => score_of::<Pebble>(1),
            Size::Medium => score_of::<Pebble>(2),
            Size::Large => score_of::<Boulder>(1),
        };
        tally.score[hit.team as usize] += points;
        let at = transform.translation;
        commands.entity(hit.target).try_despawn();
        if let Some((size, pieces)) = split(rock.size) {
            tally.splits += 1;
            for _ in 0..pieces {
                let jitter = Vec3::new(signed(&mut dice.0), signed(&mut dice.0), 0.0) * radius(size);
                spawn_rock(&mut commands, &mut dice, at + jitter, size);
            }
        }
    }
}

/// Ships that come too close to each other push apart.
fn ships_bump(mut near: MessageReader<Near<Ship, Ship>>, mut ships: Query<(&Transform, &mut Velocity), With<Ship>>) {
    for it in near.read() {
        if it.a == it.b || it.distance > 12.0 {
            continue;
        }
        let Ok([(ta, mut va), (tb, mut vb)]) = ships.get_many_mut([it.a, it.b]) else { continue };
        let push = toward(tb.translation, ta.translation).normalize_or_zero() * 4.0;
        va.0 += push;
        vb.0 -= push;
    }
}

/// When the rocks run low the field is seeded again.
fn reseed(mut commands: Commands, rocks: Res<Census<Rock>>, mut tally: ResMut<Tally>, mut dice: ResMut<Dice>) {
    if rocks.count >= ROCKS / 4 {
        return;
    }
    tally.reseeded += 1;
    for _ in 0..ROCKS / 2 {
        let at = Vec3::new(signed(&mut dice.0) * HALF, signed(&mut dice.0) * HALF, 0.0);
        spawn_rock(&mut commands, &mut dice, at, Size::Large);
    }
}

/// Ships out of fuel take some back at home.
fn refuel<T: Team>(time: Res<Time>, mut ships: Query<(&Transform, &mut Ship), With<T>>) {
    let dt = time.delta_secs().min(0.05);
    for (transform, mut ship) in &mut ships {
        if toward(transform.translation, T::HOME).length() < 80.0 {
            ship.fuel = (ship.fuel + 20.0 * dt).min(100.0);
        }
    }
}

// ---- Reporting. ----------------------------------------------------------------------------

fn at_us() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |it| it.as_micros())
}

/// Prints what `answer()` returns when it changes.
fn report(mut seen: Local<Option<u64>>, mut tally: ResMut<Tally>) {
    tally.frames += 1;
    let mut out = std::io::stdout().lock();
    let now = answer();
    if *seen != Some(now) {
        let _ = writeln!(out, "answer {now} at_us={}", at_us());
        *seen = Some(now);
    }
    let _ = out.flush();
}

/// Now and then, what the field is like, on stderr.
fn summary(tally: Res<Tally>, rocks: Res<Census<Rock>>, ships: Res<Census<Ship>>, shots: Res<Census<Shot>>) {
    if tally.frames % 20_000 == 0 {
        eprintln!(
            "frame {}: {} rocks (peak {}), {} ships, {} shots, score red {} blue {}, {} hits, {} splits, reseeded {}",
            tally.frames, rocks.count, rocks.peak, ships.count, shots.count, tally.score[0], tally.score[1],
            tally.hits, tally.splits, tally.reseeded
        );
    }
}

fn add_team<T: Team>(app: &mut App) {
    app.add_systems(Startup, spawn_team::<T>)
        .add_systems(Update, (steer::<T>, fire::<T>, refuel::<T>).chain().before(drift::<Ship>));
    eprintln!("team {} ({}) added", T::NAME, T::ID);
}

fn main() {
    let live = match blaze_live::Live::start() {
        Ok(live) => live,
        Err(why) => {
            println!("no live edits: {why}");
            std::process::exit(2);
        }
    };
    // Applied the moment it arrives, between two systems or in the middle of one, as the
    // Game of Life window does.
    live.apply_as_they_come(|outcome| {
        println!("outcome: {outcome} at_us={}", at_us());
        let _ = std::io::stdout().flush();
    });
    let mut app = App::new();
    app.add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_millis(1))))
        .init_resource::<Tally>()
        .init_resource::<Dice>()
        .init_resource::<Census<Rock>>()
        .init_resource::<Census<Ship>>()
        .init_resource::<Census<Shot>>()
        .add_message::<Hit>()
        .add_message::<Near<Shot, Rock>>()
        .add_message::<Near<Ship, Ship>>()
        .add_systems(Startup, spawn_rocks)
        .add_systems(
            Update,
            (
                (drift::<Ship>, drift::<Rock>, drift::<Shot>),
                (turn::<Rock>, wrap::<Ship>, wrap::<Rock>, wrap::<Shot>),
                (near::<Shot, Rock>, near::<Ship, Ship>),
                (shots_hit, ships_bump),
                (break_rocks, age_shots),
                (census::<Rock>, census::<Ship>, census::<Shot>),
                (reseed, summary, report),
            )
                .chain(),
        );
    add_team::<Red>(&mut app);
    add_team::<Blue>(&mut app);
    println!("ready");
    let _ = std::io::stdout().flush();
    app.run();
}
