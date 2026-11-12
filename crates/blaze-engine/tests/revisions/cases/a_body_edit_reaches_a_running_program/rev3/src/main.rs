use std::io::Write;

fn value() -> u32 {
    3
}

fn main() {
    println!("started");
    loop {
        let mut out = std::io::stdout().lock();
        writeln!(out, "value {}", value()).unwrap();
        out.flush().unwrap();
        drop(out);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
