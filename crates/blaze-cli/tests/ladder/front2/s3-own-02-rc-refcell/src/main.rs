//! s3-own-02: `Rc`, `Weak`, `RefCell`, `Cell`: counts, a parent link through `Weak` that stops
//! upgrading when the parent is dropped, a `RefCell` refusing a mutable borrow while a shared
//! one lives (`try_borrow_mut`: "Returns an error if the value is currently borrowed"),
//! `Rc::make_mut` on a unique `Rc` (no clone), `Rc::try_unwrap` while shared (`Err`).

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

struct Node {
    val: i32,
    children: RefCell<Vec<Rc<Node>>>,
    parent: RefCell<Weak<Node>>,
}

fn main() {
    let a = Rc::new(5);
    let b = Rc::clone(&a);
    println!("{} {}", Rc::strong_count(&a), *b + 1);
    {
        let _c = a.clone();
        println!("{}", Rc::strong_count(&a));
    }
    println!("{}", Rc::strong_count(&a));
    let shared = Rc::new(RefCell::new(vec![1]));
    let other = Rc::clone(&shared);
    other.borrow_mut().push(2);
    shared.borrow_mut().push(3);
    println!("{:?}", shared.borrow());
    {
        let _held = shared.borrow();
        println!("{}", shared.try_borrow_mut().is_err());
    }
    println!("{}", shared.try_borrow_mut().is_ok());
    let leaf = Rc::new(Node { val: 3, children: RefCell::new(vec![]), parent: RefCell::new(Weak::new()) });
    println!("{:?}", leaf.parent.borrow().upgrade().map(|p| p.val));
    {
        let branch = Rc::new(Node { val: 5, children: RefCell::new(vec![Rc::clone(&leaf)]), parent: RefCell::new(Weak::new()) });
        *leaf.parent.borrow_mut() = Rc::downgrade(&branch);
        let up = leaf.parent.borrow().upgrade().map(|p| p.val);
        println!("{:?} {} {}", up, Rc::strong_count(&branch), Rc::weak_count(&branch));
        println!("{} {}", branch.children.borrow().len(), Rc::strong_count(&leaf));
    }
    println!("{:?} {}", leaf.parent.borrow().upgrade().map(|p| p.val), Rc::strong_count(&leaf));
    let c = Cell::new(1);
    c.set(c.get() + 41);
    println!("{}", c.replace(0));
    let mut unique = Rc::new(String::from("x"));
    Rc::make_mut(&mut unique).push('y');
    println!("{unique}");
    let r1 = Rc::new(1);
    let r2 = r1.clone();
    println!("{}", Rc::ptr_eq(&r1, &r2));
    println!("{:?}", Rc::try_unwrap(r1));
    println!("{}", r2);
}
