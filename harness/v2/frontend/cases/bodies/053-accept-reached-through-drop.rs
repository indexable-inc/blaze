//@ expect: accept
struct Guard(&'static str);

impl Drop for Guard {
    fn drop(&mut self) {
        println!("dropped {}", self.0);
    }
}

fn main() {
    let _a = Guard("a");
    {
        let _b = Guard("b");
    }
    println!("end of main");
}
