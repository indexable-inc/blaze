//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn find(limit: u32) -> u32 {
    let _fn = D("fn");
    for i in 0.. {
        let _loop = D("loop");
        match i {
            0 => {
                let _zero = D("zero");
            }
            n if n >= limit => {
                let _found = D("found");
                return n;
            }
            _ => {}
        }
    }
    unreachable!()
}

fn main() {
    println!("{}", find(2));
}
