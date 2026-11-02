use blaze_serde::{
    Deserialize, Serialize,
    serde::de::value::{Error, MapDeserializer},
};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Point {
    first_x: i32,
    y: i32,
    label: Option<i32>,
}

fn main() {
    let fields = [("y", -4), ("first-x", 3)];
    let point = Point::deserialize(MapDeserializer::<_, Error>::new(fields.into_iter())).unwrap();
    println!("{} {} {:?}", point.first_x, point.y, point.label);
}
