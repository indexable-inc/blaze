use std::io::Write;

fn value() -> u32 {
    2
}

fn main() {
    // A run after the first (each edit's `blaze run`, with `ONCE` set) prints its value once
    // and ends; the first keeps running, and its output is what the edit must reach.
    if std::env::var_os("ONCE").is_some() {
        println!("once {}", value());
        return;
    }
    println!("started");
    loop {
        let mut out = std::io::stdout().lock();
        writeln!(out, "value {}", value()).unwrap();
        out.flush().unwrap();
        drop(out);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
