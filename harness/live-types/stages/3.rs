
/// What `V1` became; `V1` is gone.
#[derive(Debug, Clone, Copy, PartialEq)]
struct V2 {
    x: u64,
    tag: u8,
}

component!(V2);

#[inline(never)]
fn tick(world: &mut World) -> String {
    if world.columns.is_empty() {
        for x in 1..=3u8 {
            world.add(V2 { x: u64::from(x) * 10, tag: b'a' + x });
        }
    }
    let (mut total, mut tags) = (0, Vec::new());
    world.each::<V2>(|it| {
        total += it.x;
        tags.push(it.tag as char);
    });
    format!("stage 3: v2 total {total}, tags {tags:?}")
}
