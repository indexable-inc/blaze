use std::{
    fs::OpenOptions,
    io::Write,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[inline(never)]
fn answer() -> u64 {
    1
}

fn main() {
    // Patches from `dx serve --hotpatch`; started any other way this does nothing.
    dioxus_devtools::connect_subsecond();
    // dx keeps the program's output to itself, so it is written to a file of the harness's.
    let path = std::env::var("HOTPROBE_LOG").expect("HOTPROBE_LOG names the file to write to");
    let mut log = OpenOptions::new().create(true).append(true).open(path).unwrap();
    writeln!(log, "ready pid={}", std::process::id()).unwrap();
    let mut seen = 0;
    loop {
        let now = subsecond::call(answer);
        if now != seen {
            let at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
            writeln!(log, "answer {now} at_us={at} pid={}", std::process::id()).unwrap();
            seen = now;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
