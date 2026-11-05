mod report;

use sbase::shapes::{describe_all, total_area, Area, Describe, Point, Shape};
use sbase::stack::Stack;
use smid::Tile;

fn local_sum(items: &[u32]) -> u32 {
    items.iter().sum()
}

fn guarded() -> String {
    let caught = std::panic::catch_unwind(|| {
        let v: Vec<u32> = Vec::new();
        v[3]
    });
    match caught {
        Ok(_) => "no panic".to_string(),
        Err(_) => "panicked".to_string(),
    }
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        println!("panic at {at}");
    }));
    let shapes = vec![Shape::Circle(2), Shape::Rect { w: 3, h: 4 }, Shape::Dot];
    report::line("total_area", total_area(&shapes));
    report::line("doubled", shapes[1].doubled());
    let tile = Tile { side: 5, label: "t".to_string() };
    report::line("tile_area", tile.area());
    report::line("tile_debug", format!("{tile:?}"));
    report::line("tile_default", format!("{:?}", Tile::default()));
    report::line("tile_cmp", tile > Tile::default());
    report::line("sides", smid::sides_of(&tile) + smid::sides_of(&shapes[0]));
    let all: Vec<Box<dyn Describe>> = vec![Box::new(Shape::Dot), Box::new(Point::new(1, -2)), Box::new(tile.clone())];
    report::line("describe", describe_all(&all));
    report::line("manhattan", Point::new(3, -4).manhattan());
    let mut stack = Stack::new();
    stack.push(1u8);
    stack.push(9u8);
    report::line("stack", format!("{:?} {}", stack.top_twice(), stack.len()));
    let mut names: Stack<String> = Stack::default();
    names.push("x".to_string());
    report::line("names", format!("{:?}", names.pop()));
    report::line("scaled", sbase::scaled(7));
    const AT_COMPILE: u32 = sbase::scaled(2);
    report::line("const", AT_COMPILE + sbase::TABLE.len() as u32);
    report::line("count", sbase::count() + sbase::count());
    report::line("greeting", sbase::GREETING);
    report::line("adders", sbase::add_two(1) + smid::add_seven(1));
    report::line("digest", sbase::digest(b"stress"));
    report::line("largest", format!("{} {}", sbase::largest(&[3, 9, 4]), sbase::largest(&[1.5, 0.5])));
    report::line("checked", format!("{:?} {}", smid::checked(5).ok(), smid::checked(500).unwrap_err()));
    report::line("closures", smid::multiplier(3)(4) + smid::boxed_op(1)(1) + smid::boxed_op(2)(2));
    report::line("drops", smid::drops());
    report::line("seeded", smid::seeded() + smid::seeded());
    report::line("evens", smid::Evens::up_to(9).map(|x| x.to_string()).collect::<Vec<_>>().join(""));
    report::line("text", format!("{} {}", smid::text::shout("hi"), smid::text::initials("a big cat")));
    report::line("async", smid::tasks::block_on(smid::tasks::pipeline(1)));
    report::line("local", local_sum(&[1, 2, 3]));
    report::line("lines", format!("{} {}", sbase::where_am_i(), sbase::caller_line()));
    report::line("guarded", guarded());
}
