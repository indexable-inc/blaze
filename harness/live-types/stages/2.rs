
#[derive(Debug, Clone, Copy, PartialEq)]
struct V1 {
    x: u32,
}

component!(V1);

/// What `V1` becomes: a type of its own, added while the program runs.
#[derive(Debug, Clone, Copy, PartialEq)]
struct V2 {
    x: u64,
    tag: u8,
}

component!(V2);

impl From<V1> for V2 {
    fn from(it: V1) -> V2 {
        V2 { x: u64::from(it.x) * 10, tag: b'a' + it.x as u8 }
    }
}

/// How many times the values were converted: a static new in the edit, which starts at zero.
static MIGRATED: AtomicU32 = AtomicU32::new(0);

#[inline(never)]
fn tick(world: &mut World) -> String {
    if world.columns.is_empty() {
        for x in 1..=3 {
            world.add(V1 { x });
        }
    }
    let old = world.take::<V1>();
    if !old.is_empty() {
        MIGRATED.fetch_add(1, Ordering::Relaxed);
        for it in old {
            world.add(V2::from(it));
        }
    }
    let (mut total, mut first) = (0, None);
    world.each::<V2>(|it| {
        total += it.x;
        first.get_or_insert(*it);
    });
    format!(
        "stage 2: v2 total {total}, first {:?}, V1 index {}, V2 index {}, migrated {}",
        first.expect("the values were converted"),
        V1::index(),
        V2::index(),
        MIGRATED.load(Ordering::Relaxed)
    )
}
