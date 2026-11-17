//! s3-own-04: the order of drops, printed. The Reference, "Destructors":
//! - "When control flow leaves a drop scope all variables associated to that scope are dropped in
//!   reverse order of declaration";
//! - "The fields of a struct are dropped in declaration order"; tuple fields in order; an array's
//!   or `Vec`'s elements from first to last (`drop_in_place` on a slice);
//! - a temporary is dropped at the end of its enclosing statement, so `let _ = make(..)` drops at
//!   once and a temporary in `println!`'s arguments drops after the line is written;
//! - assigning to a place drops its old value first ("Assignment expressions": "evaluates its
//!   right hand operand first ... then drops the old value");
//! - a moved-from local is not dropped; `mem::drop` drops at the call.

struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

struct Pair {
    _first: D,
    _second: D,
}

fn make(name: &'static str) -> D {
    D(name)
}

fn main() {
    {
        let _a = D("a");
        let _b = D("b");
        println!("scope end");
    }
    {
        let _p = Pair { _first: D("f1"), _second: D("f2") };
    }
    {
        let _v = vec![D("v0"), D("v1")];
    }
    {
        let _t = (D("t0"), D("t1"));
    }
    let x = D("x");
    std::mem::drop(x);
    println!("after mem::drop");
    let _ = make("temp-underscore");
    println!("after let _");
    let _kept = make("kept");
    println!("after let _kept");
    println!("len {}", make("temp-in-expr").0.len());
    let mut o = Some(D("opt"));
    o.take();
    println!("after take {}", o.is_none());
    let mut r = D("old");
    println!("holding {}", r.0);
    r = D("new");
    println!("after assign {}", r.0);
    let moved = D("moved");
    let keep = moved;
    println!("moved once {}", keep.0);
    println!("end");
}
