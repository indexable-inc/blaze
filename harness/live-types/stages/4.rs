
/// What `V1` became, changed in place: a running program has values of it laid out as before.
#[derive(Debug, Clone, Copy, PartialEq)]
struct V2 {
    x: u32,
    tag: u8,
}

component!(V2);

#[inline(never)]
fn tick(world: &mut World) -> String {
    if world.columns.is_empty() {
        for x in 1..=3u8 {
            world.add(V2 { x: u32::from(x) * 10, tag: b'a' + x });
        }
    }
    let (mut total, mut tags) = (0, Vec::new());
    world.each::<V2>(|it| {
        total += it.x;
        tags.push(it.tag as char);
    });
    format!("stage 4: v2 total {total}, tags {tags:?}")
}
