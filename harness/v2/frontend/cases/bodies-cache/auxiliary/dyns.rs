//@ edition: 2021
use std::any::Any;
pub trait Animal { fn name(&self) -> String; fn legs(&self) -> u32 { 4 } }
pub struct Dog;
pub struct Bird;
impl Animal for Dog { fn name(&self) -> String { "dog".into() } }
impl Animal for Bird { fn name(&self) -> String { "bird".into() } fn legs(&self) -> u32 { 2 } }
pub fn zoo() -> Vec<Box<dyn Animal>> { vec![Box::new(Dog), Box::new(Bird)] }
pub fn describe(x: &dyn Any) -> String {
    if let Some(n) = x.downcast_ref::<u32>() { format!("u32 {n}") }
    else if let Some(s) = x.downcast_ref::<&str>() { format!("str {s}") }
    else { "other".to_string() }
}
