#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub fn new(x: i32, y: i32) -> Point {
        Point { x, y }
    }

    pub fn manhattan(&self) -> i32 {
        self.x.abs() + self.y.abs()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Circle(u32),
    Rect { w: u32, h: u32 },
    Dot,
}

pub trait Area {
    const SIDES: u32;

    fn area(&self) -> u32;

    fn doubled(&self) -> u32 {
        self.area() * 2
    }
}

impl Area for Shape {
    const SIDES: u32 = 0;

    fn area(&self) -> u32 {
        match self {
            Shape::Circle(r) => 3 * r * r,
            Shape::Rect { w, h } => w * h,
            Shape::Dot => 0,
        }
    }
}

pub trait Describe {
    fn name(&self) -> String;

    fn describe(&self) -> String {
        format!("<{}>", self.name())
    }
}

impl Describe for Shape {
    fn name(&self) -> String {
        match self {
            Shape::Circle(_) => "circle".to_string(),
            Shape::Rect { .. } => "rect".to_string(),
            Shape::Dot => "dot".to_string(),
        }
    }
}

impl Describe for Point {
    fn name(&self) -> String {
        format!("point({},{})", self.x, self.y)
    }
}

pub fn total_area<T: Area>(items: &[T]) -> u32 {
    items.iter().map(|it| it.area()).sum()
}

pub fn describe_all(items: &[Box<dyn Describe>]) -> String {
    items.iter().map(|it| it.describe()).collect::<Vec<_>>().join(",")
}
