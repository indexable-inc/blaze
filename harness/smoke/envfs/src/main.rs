use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::{env, process};

fn scratch(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("blaze-smoke-{name}-{}", process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The sorted names of `dir`'s entries.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn main() {
    println!("var {:?}", env::var("SMOKE_VAR"));
    println!("unset {}", env::var("SMOKE_UNSET").is_err());
    let args: Vec<String> = env::args().skip(1).collect();
    println!("args {args:?}");
    println!("cwd has Cargo.toml {}", env::current_dir().unwrap().join("Cargo.toml").is_file());

    let dir = scratch("main");
    let file = dir.join("a.txt");
    fs::write(&file, "hello\n").unwrap();
    fs::OpenOptions::new().append(true).open(&file).unwrap().write_all(b"world\n").unwrap();
    fs::rename(&file, dir.join("b.txt")).unwrap();
    fs::create_dir(dir.join("sub")).unwrap();
    fs::write(dir.join("sub").join("c.txt"), [1u8, 2, 3]).unwrap();
    println!("entries {:?}", entries(&dir));
    println!("b.txt {:?}", fs::read_to_string(dir.join("b.txt")).unwrap());
    println!("c.txt {} bytes", fs::metadata(dir.join("sub").join("c.txt")).unwrap().len());
    fs::remove_dir_all(&dir).unwrap();
    println!("removed {}", !dir.exists());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package() {
        assert_eq!(env!("CARGO_PKG_NAME"), "envfs");
        assert!(Path::new(env!("CARGO_MANIFEST_DIR")).join("expected.txt").is_file());
    }

    #[test]
    fn round_trip() {
        let dir = scratch("test");
        fs::write(dir.join("x"), "x").unwrap();
        assert_eq!(entries(&dir), ["x"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
