//! The middle crate: its own types over the bottom crate's traits, closures, drops, async.
pub mod text;
pub mod tasks;

use sbase::shapes::{Area, Describe};
use std::cell::RefCell;
use std::fmt;

#[derive(Debug, Clone, PartialEq, PartialOrd, Default, Hash)]
pub struct Tile {
    pub side: u32,
    pub label: String,
}

impl Area for Tile {
    const SIDES: u32 = 4;

    fn area(&self) -> u32 {
        sbase::square!(self.side)
    }
}

impl Describe for Tile {
    fn name(&self) -> String {
        format!("tile:{}", self.label)
    }
}

#[derive(Debug)]
pub enum Failure {
    Empty,
    TooBig(u32),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Empty => write!(f, "nothing there"),
            Failure::TooBig(n) => write!(f, "{n} is too big"),
        }
    }
}

impl std::error::Error for Failure {}

pub fn checked(n: u32) -> Result<u32, Failure> {
    if n == 0 {
        return Err(Failure::Empty);
    }
    if n > 100 {
        return Err(Failure::TooBig(n));
    }
    Ok(n + 1)
}

pub fn multiplier(by: u32) -> impl Fn(u32) -> u32 {
    move |x| x * by
}

pub fn boxed_op(kind: u8) -> Box<dyn Fn(u32) -> u32> {
    match kind {
        0 => Box::new(|x| x + 1),
        1 => Box::new(sbase::add_two),
        _ => Box::new(multiplier(5)),
    }
}

thread_local! {
    pub static LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

thread_local! {
    static SEED: std::cell::Cell<u32> = const { std::cell::Cell::new(4) };
}

pub fn seeded() -> u32 {
    SEED.with(|seed| {
        seed.set(seed.get() * 3);
        seed.get()
    })
}

pub struct Noisy(pub &'static str);

impl Drop for Noisy {
    fn drop(&mut self) {
        LOG.with(|log| log.borrow_mut().push(format!("drop {}", self.0)));
    }
}

pub fn drops() -> String {
    {
        let _a = Noisy("a");
        let _b = Noisy("b");
    }
    LOG.with(|log| log.borrow_mut().drain(..).collect::<Vec<_>>().join(";"))
}

pub struct Evens {
    next: u32,
    end: u32,
}

impl Evens {
    pub fn up_to(end: u32) -> Evens {
        Evens { next: 0, end }
    }
}

impl Iterator for Evens {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if self.next > self.end {
            return None;
        }
        let now = self.next;
        self.next += 2;
        Some(now)
    }
}

sbase::make_adder!(add_seven, 7);

pub fn sides_of<T: Area>(_: &T) -> u32 {
    T::SIDES
}
