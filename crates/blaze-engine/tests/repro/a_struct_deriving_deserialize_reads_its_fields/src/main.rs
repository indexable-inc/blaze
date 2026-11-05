use serde::Deserialize;
use serde::de::value::{Error, MapDeserializer};

#[derive(Deserialize)]
struct Point {
    x: i32,
    y: i32,
    #[serde(default)]
    label: Option<i32>,
}

fn main() {
    let fields = [("y", -4), ("x", 3)];
    let point = Point::deserialize(MapDeserializer::<_, Error>::new(fields.into_iter())).unwrap();
    println!("{} {} {:?}", point.x, point.y, point.label);
}
