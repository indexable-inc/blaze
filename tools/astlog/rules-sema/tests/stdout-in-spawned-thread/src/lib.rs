//! The shape of agents/annoyances/vac-3.json: a spawned thread writes to standard output while
//! the command's own thread holds its lock, so the write waits for the command to end.

use std::io::Write as _;

fn report(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
}

fn progress(done: u32) {
    report(&done.to_string());
}

// Found: the thread's closure reaches `stdout` two calls down.
pub fn watch() -> std::thread::JoinHandle<()> {
    std::thread::spawn(|| {
        for done in 0..3 {
            progress(done);
        }
    })
}

// Found: the closure takes the stream itself.
pub fn shout() -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let _ = std::io::stdout().write_all(b"hi\n");
    })
}

fn count(n: u32) -> u32 {
    n + 1
}

// Not found: the thread computes and the command's thread prints.
pub fn computed() -> u32 {
    let handle = std::thread::spawn(|| count(1));
    let n = handle.join().unwrap_or(0);
    report(&n.to_string());
    n
}

// Not found: standard error is unbuffered and unlocked by the command.
pub fn warn() -> std::thread::JoinHandle<()> {
    std::thread::spawn(|| {
        let _ = std::io::stderr().write_all(b"careful\n");
    })
}

pub struct Log;

impl Log {
    fn line(&self, text: &str) {
        report(text);
    }
}

// Found: the closure reaches `stdout` through a method call.
pub fn logged(log: Log) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || log.line("started"))
}
