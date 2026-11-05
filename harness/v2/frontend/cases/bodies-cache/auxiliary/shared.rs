//@ edition: 2021
use std::cell::RefCell;
use std::rc::Rc;
pub struct Counter { pub hits: RefCell<u32> }
pub fn shared() -> (Rc<Counter>, Rc<Counter>) { let c = Rc::new(Counter { hits: RefCell::new(0) }); (c.clone(), c) }
pub fn hit(c: &Counter) { *c.hits.borrow_mut() += 1; }
