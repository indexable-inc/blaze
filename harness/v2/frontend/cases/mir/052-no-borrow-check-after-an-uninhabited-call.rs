//@ expect: accept
#![allow(unreachable_code)]

enum Never {}

struct S;

impl S {
    fn end(&self) -> Never {
        println!("end");
        std::process::exit(0)
    }
}

fn consume(s: String) {
    println!("{s}");
}

fn main() {
    let s = String::from("moved");
    let x = S.end();
    let _ = x;
    consume(s);
    consume(s);
}
