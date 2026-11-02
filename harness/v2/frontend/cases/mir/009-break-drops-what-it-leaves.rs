//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn main() {
    let _outside = D("outside");
    let mut n = 0;
    let value = loop {
        let _loop = D("loop");
        n += 1;
        {
            let _inner = D("inner");
            if n == 3 {
                let _last = D("last");
                break n * 10;
            }
        }
        println!("next {n}");
    };
    println!("value {value}");
}
