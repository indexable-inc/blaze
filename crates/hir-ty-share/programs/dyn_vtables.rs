// A coercion to `dyn Trait` makes the vtable of its source type: `erase::<P>` and
// `erase::<Q>` must not share, though `&P` and `&Q` have one layout.
trait Show {
    fn show(&self) -> String;
}

struct P(u32);
struct Q(u32);

impl Show for P {
    fn show(&self) -> String {
        format!("P{}", self.0)
    }
}
impl Show for Q {
    fn show(&self) -> String {
        format!("Q{}", self.0)
    }
}
impl<T: Show + ?Sized> Show for &T {
    fn show(&self) -> String {
        format!("&{}", (**self).show())
    }
}

fn erase<T: Show>(x: &T) -> &dyn Show {
    x
}

fn boxed<T: Show + 'static>(x: T) -> Box<dyn Show> {
    Box::new(x)
}

fn call<T: Show>(x: &T) -> String {
    x.show()
}

fn main() {
    let (p, q) = (P(1), Q(2));
    for shown in [erase(&p), erase(&q)] {
        println!("{}", shown.show());
    }
    for shown in [boxed(P(3)), boxed(Q(4))] {
        println!("{}", shown.show());
    }
    println!("{} {}", call(&p), call(&q));
    println!("{} {}", call(&&p), call(&&q));
}
