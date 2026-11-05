//! A bevy app without a window: `MinimalPlugins` stepped by hand through a fixed number of
//! updates, with systems, events, change detection, commands and a hierarchy of entities.

use bevy::prelude::*;

#[derive(Component, Clone, Copy)]
struct Position(Vec2);

#[derive(Component, Clone, Copy)]
struct Velocity(Vec2);

#[derive(Component)]
struct Lifetime(u32);

#[derive(Component)]
struct Spawner {
    every: u32,
}

#[derive(Event)]
struct Bounced {
    entity: Entity,
    wall: u8,
}

#[derive(Resource, Default)]
struct Tally {
    ticks: u32,
    bounces: [u32; 4],
    spawned: u32,
    expired: u32,
    moved: u64,
}

#[derive(Resource)]
struct Lcg(u64);

impl Lcg {
    fn unit(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

const BOUNDS: f32 = 100.0;
const STEP: f32 = 1.0 / 64.0;

fn setup(mut commands: Commands, mut lcg: ResMut<Lcg>) {
    for n in 0..200 {
        let place = Vec2::new(lcg.unit(), lcg.unit()) * BOUNDS;
        let speed = (Vec2::new(lcg.unit(), lcg.unit()) - 0.5) * 400.0;
        let mut entity = commands.spawn((Position(place), Velocity(speed)));
        if n % 20 == 0 {
            entity.insert(Spawner { every: 7 + n / 20 }).with_children(|parent| {
                parent.spawn(Lifetime(u32::MAX));
            });
        }
    }
}

fn movement(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut position, velocity) in &mut query {
        position.0 += velocity.0 * STEP;
    }
}

fn bounce(mut query: Query<(Entity, &mut Position, &mut Velocity)>, mut events: EventWriter<Bounced>) {
    for (entity, mut position, mut velocity) in &mut query {
        for axis in 0..2 {
            let at = position.0[axis];
            if !(0.0..=BOUNDS).contains(&at) {
                position.0[axis] = at.clamp(0.0, BOUNDS);
                velocity.0[axis] = -velocity.0[axis] * 0.98;
                events.send(Bounced { entity, wall: (axis * 2) as u8 + u8::from(at > 0.0) });
            }
        }
    }
}

fn count_bounces(mut events: EventReader<Bounced>, mut tally: ResMut<Tally>, spawners: Query<&Spawner>) {
    for bounced in events.read() {
        tally.bounces[bounced.wall as usize] += 1 + u32::from(spawners.contains(bounced.entity));
    }
}

fn spawn(mut commands: Commands, mut tally: ResMut<Tally>, query: Query<(&Position, &Velocity, &Spawner)>) {
    tally.ticks += 1;
    for (position, velocity, spawner) in &query {
        if tally.ticks % spawner.every == 0 {
            commands.spawn((*position, Velocity(velocity.0.perp() * 0.5), Lifetime(40 + spawner.every)));
            tally.spawned += 1;
        }
    }
}

fn expire(mut commands: Commands, mut tally: ResMut<Tally>, mut query: Query<(Entity, &mut Lifetime), With<Position>>) {
    for (entity, mut lifetime) in &mut query {
        lifetime.0 -= 1;
        if lifetime.0 == 0 {
            commands.entity(entity).despawn();
            tally.expired += 1;
        }
    }
}

fn changed(query: Query<(), Changed<Position>>, mut tally: ResMut<Tally>) {
    tally.moved += query.iter().count() as u64;
}

fn main() {
    let updates: u32 = std::env::args().nth(1).and_then(|it| it.parse().ok()).unwrap_or(300);
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(Lcg(9))
        .init_resource::<Tally>()
        .add_event::<Bounced>()
        .add_systems(Startup, setup)
        .add_systems(Update, (movement, bounce, count_bounces, spawn, expire, changed).chain());
    for _ in 0..updates {
        app.update();
    }
    let world = app.world_mut();
    let tally = world.resource::<Tally>();
    println!("ticks {} bounces {:?} spawned {} expired {} moved {}", tally.ticks, tally.bounces, tally.spawned, tally.expired, tally.moved);
    let mut positions = world.query::<(Entity, &Position, &Velocity)>();
    let mut rows: Vec<(Entity, Vec2, Vec2)> = positions.iter(world).map(|(e, p, v)| (e, p.0, v.0)).collect();
    rows.sort_by_key(|row| row.0);
    let sum = rows.iter().fold(Vec2::ZERO, |sum, row| sum + row.1);
    let energy: f32 = rows.iter().map(|row| row.2.length_squared()).sum();
    println!("{} entities, centre {:.3} {:.3}, energy {:.1}", rows.len(), sum.x / rows.len() as f32, sum.y / rows.len() as f32, energy);
    for row in rows.iter().step_by(rows.len() / 8 + 1) {
        println!("  {:?} at {:.4} {:.4}", row.0, row.1.x, row.1.y);
    }
    let mut parents = world.query::<(&Children, &Spawner)>();
    println!("{} spawners with children", parents.iter(world).count());
}
