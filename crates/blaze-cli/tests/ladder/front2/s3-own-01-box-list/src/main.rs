//! s3-own-01: `Box`: a recursive enum, `Box<dyn Trait>` and its drop, a large boxed array,
//! `Box<[T]>`, `Box<str>`, `Box::into_raw`/`from_raw`, and the drops left at the end of `main`.
//!
//! The Reference, "Destructors": locals are dropped in reverse order of declaration; a moved-from
//! local is not dropped; a `Vec`'s elements are dropped with it. So after "end of main" `holder`
//! (declared last of the two live owners) drops `moved` first, then `back` drops `raw`.

#[derive(Debug)]
enum List {
    Cons(i32, Box<List>),
    Nil,
}

use List::{Cons, Nil};

impl List {
    fn sum(&self) -> i32 {
        let mut s = 0;
        let mut at = self;
        while let Cons(v, next) = at {
            s += v;
            at = next;
        }
        s
    }
}

struct Noisy(&'static str);

impl Drop for Noisy {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

trait Speak {
    fn say(&self) -> String;
}

impl Speak for Noisy {
    fn say(&self) -> String {
        format!("I am {}", self.0)
    }
}

fn main() {
    let l = Cons(1, Box::new(Cons(2, Box::new(Cons(3, Box::new(Nil))))));
    println!("{:?}", l);
    println!("{}", l.sum());
    let b: Box<dyn Speak> = Box::new(Noisy("boxed"));
    println!("{}", b.say());
    drop(b);
    let big = Box::new([7u8; 4096]);
    println!("{}", big.iter().map(|&x| x as u32).sum::<u32>());
    let slice: Box<[i32]> = vec![1, 2, 3].into_boxed_slice();
    println!("{:?} {}", slice, slice.len());
    let s: Box<str> = "boxed str".into();
    println!("{}", s.to_uppercase());
    let raw = Box::into_raw(Box::new(Noisy("raw")));
    // SAFETY: `raw` came from `Box::into_raw` just above and is turned back into a box once.
    let back = unsafe { Box::from_raw(raw) };
    println!("{}", back.say());
    let moved = Box::new(Noisy("moved"));
    let holder = vec![moved];
    println!("{}", holder.len());
    println!("end of main");
}
