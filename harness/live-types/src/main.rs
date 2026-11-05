//! A program whose components change while it runs, as a game's do under live edits: a
//! component's type is never changed in place. `harness/live_types.py` gives it the texts of
//! `stages/` one after another: a new type `V2` beside `V1`, the code moved to `V2` with the
//! values converted, `V1` deleted. Everything above `// The stage.` is the same in every one.

use std::{
    any::Any,
    io::Write,
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// A type a world keeps values of, as flecs_ecs registers one: its id is handed out the first
/// time it is asked for, from a pool every type shares, and kept in a static of its own.
trait Component: Any {
    fn index() -> u32;

    fn fetch_new_index() -> u32 {
        static POOL: AtomicU32 = AtomicU32::new(0);
        POOL.fetch_add(1, Ordering::Relaxed)
    }

    fn get_or_init_index(id: &AtomicU32) -> u32 {
        match id.fetch_update(Ordering::Acquire, Ordering::Relaxed, |it| {
            (it == u32::MAX).then(Self::fetch_new_index)
        }) {
            Ok(_) => id.load(Ordering::Acquire),
            Err(old) => old,
        }
    }
}

/// What `#[derive(Component)]` writes.
macro_rules! component {
    ($ty:ty) => {
        impl Component for $ty {
            fn index() -> u32 {
                static INDEX: AtomicU32 = AtomicU32::new(u32::MAX);
                Self::get_or_init_index(&INDEX)
            }
        }
    };
}

/// Values by the id of their type, which is erased: the world outlives every type it holds.
struct World {
    columns: Vec<(u32, Vec<Box<dyn Any>>)>,
}

impl World {
    fn add<T: Component>(&mut self, value: T) {
        let index = T::index();
        match self.columns.iter_mut().find(|it| it.0 == index) {
            Some(column) => column.1.push(Box::new(value)),
            None => self.columns.push((index, vec![Box::new(value)])),
        }
    }

    fn each<T: Component>(&mut self, mut f: impl FnMut(&mut T)) {
        let index = T::index();
        for column in self.columns.iter_mut().filter(|it| it.0 == index) {
            for value in &mut column.1 {
                f(value.downcast_mut::<T>().expect("a column holds values of its type"));
            }
        }
    }

    /// Takes every value of `T` out of the world.
    fn take<T: Component>(&mut self) -> Vec<T> {
        let index = T::index();
        let Some(at) = self.columns.iter().position(|it| it.0 == index) else {
            return Vec::new();
        };
        let column = self.columns.remove(at).1;
        column.into_iter().map(|it| *it.downcast::<T>().expect("a column holds values of its type")).collect()
    }
}

fn main() {
    match blaze_live::Live::start() {
        // Applied the moment it arrives, as a game's window does.
        Ok(live) => live.apply_as_they_come(|outcome| {
            println!("outcome: {outcome}");
            std::io::stdout().flush().unwrap();
        }),
        Err(why) => println!("no live edits: {why}"),
    }
    println!("ready");
    let mut world = World { columns: Vec::new() };
    let mut seen = String::new();
    loop {
        let now = tick(&mut world);
        if now != seen {
            let at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
            println!("answer {now} at_us={at}");
            seen = now;
        }
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
}

// The stage.

#[derive(Debug, Clone, Copy, PartialEq)]
struct V1 {
    x: u32,
}

component!(V1);

#[inline(never)]
fn tick(world: &mut World) -> String {
    if world.columns.is_empty() {
        for x in 1..=3 {
            world.add(V1 { x });
        }
    }
    let mut total = 0;
    world.each::<V1>(|it| total += it.x);
    format!("stage 0: v1 total {total}, V1 index {}", V1::index())
}
