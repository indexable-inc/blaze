//! s2-gen-01: trait objects over the real library: `Box<dyn Trait>` in a `Vec`, provided methods,
//! `&dyn Trait` arrays, `impl Trait` arguments, `&dyn Debug`, and `dyn Any` downcasting
//! (`Any::type_id` through the vtable; core/src/any.rs).

use std::any::Any;
use std::fmt::Debug;

trait Shape {
    fn area(&self) -> f64;
    fn name(&self) -> String {
        String::from("shape")
    }
}

struct Sq(f64);

struct Rect {
    w: f64,
    h: f64,
}

impl Shape for Sq {
    fn area(&self) -> f64 {
        self.0 * self.0
    }
    fn name(&self) -> String {
        format!("square {}", self.0)
    }
}

impl Shape for Rect {
    fn area(&self) -> f64 {
        self.w * self.h
    }
}

fn total(shapes: &[Box<dyn Shape>]) -> f64 {
    shapes.iter().map(|s| s.area()).sum()
}

fn biggest(shapes: &[Box<dyn Shape>]) -> &dyn Shape {
    &**shapes.iter().max_by(|a, b| a.area().total_cmp(&b.area())).unwrap()
}

fn describe(s: &impl Shape) -> String {
    format!("{} has area {}", s.name(), s.area())
}

fn make(n: u8) -> Box<dyn Shape> {
    if n == 0 { Box::new(Sq(2.0)) } else { Box::new(Rect { w: 1.5, h: 4.0 }) }
}

trait Animal {
    fn speak(&self) -> &'static str;
}

struct Dog;
struct Cat;

impl Animal for Dog {
    fn speak(&self) -> &'static str {
        "woof"
    }
}

impl Animal for Cat {
    fn speak(&self) -> &'static str {
        "meow"
    }
}

fn main() {
    let shapes: Vec<Box<dyn Shape>> = vec![make(0), make(1), Box::new(Sq(1.0))];
    println!("{}", total(&shapes));
    for s in &shapes {
        println!("{} {}", s.name(), s.area());
    }
    println!("{}", biggest(&shapes).name());
    println!("{}", describe(&Sq(3.0)));
    let zoo: [&dyn Animal; 2] = [&Dog, &Cat];
    println!("{}", zoo.iter().map(|a| a.speak()).collect::<Vec<_>>().join(" "));
    let d: &dyn Debug = &vec![1, 2];
    println!("{:?}", d);
    let any: Box<dyn Any> = Box::new(5u32);
    println!("{:?} {:?}", any.downcast_ref::<u32>(), any.downcast_ref::<i32>());
}
