//@ expect: reject E0713
struct Guard<'a> {
    data: &'a mut String,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.data.push('!');
    }
}

fn demo<'a>(g: Guard<'a>) -> &'a mut String {
    let p = &mut *g.data; //~ ERROR E0713
    p
}

fn main() {
    let mut s = String::new();
    let r = demo(Guard { data: &mut s });
    r.push('x');
    println!("{s}");
}
