//@ expect: accept
#![warn(unreachable_code)]

enum Void {}

struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn stop() -> Void {
    println!("stopping");
    std::process::exit(0)
}

fn helper() -> u32 {
    println!("never printed");
    1
}

fn main() {
    let _guard = D("guard");
    println!("before");
    if std::env::args().count() > 100 {
        let x = stop();
        let _ = x;
    }
    stop();
    helper();
    println!("after");
}
