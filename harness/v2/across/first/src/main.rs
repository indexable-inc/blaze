//! Reaches the standard library's allocation and path code one way: through the environment,
//! a child process and a file written.
use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    let out = PathBuf::from(env::var_os("TMPDIR").unwrap_or_else(|| "/tmp".into()));
    let name = env::var("USER").unwrap_or_default();
    let text = "hello, $$\n".replace("$$", &name);
    fs::write(out.join("across-first.txt"), text).unwrap();
    let said = Command::new("true").arg("--version").output().ok();
    let words = said.as_ref().and_then(|it| std::str::from_utf8(&it.stdout).ok()).unwrap_or("");
    let mut pieces = words.split('.');
    println!("{:?} {:?}", pieces.next(), pieces.next().and_then(|it| it.parse::<u32>().ok()));
}
