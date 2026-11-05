
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

#[inline(never)]
fn tick(world: &mut World) -> String {
    if world.columns.is_empty() {
        for x in 1..=3 {
            world.add(V1 { x });
        }
    }
    let mut total = 0;
    world.each::<V1>(|it| total += it.x);
    format!("stage 1: v1 total {total}, V1 index {}, V2 index {}", V1::index(), V2::index())
}
