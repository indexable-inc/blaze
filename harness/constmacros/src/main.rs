#[cfg(not(blaze_const_macros))]
compile_error!("shapes' macros are const macros, which blaze runs: build it with `blaze build`");

pub trait Named {
    const NAME: &'static str;
}

pub trait Fields {
    const FIELDS: &'static [&'static str];
}

#[derive(macros::Named, macros::Fields)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

fn main() {
    let point = Point { x: 1, y: 2 };
    println!("{} {} {}", Point::NAME, point.x + point.y, Point::FIELDS.join(","));
}
