//! s7-fs-05: files under the system temp directory, in a directory this program makes and
//! removes (`std::env::temp_dir()`: `TMPDIR`, else on Apple `confstr(_CS_DARWIN_USER_TEMP_DIR)`,
//! else "/tmp": std/src/sys/paths/unix.rs). The directory's name holds the process id, so two
//! runs at once never share it; no path is printed. `read_dir`'s order is the file system's, so
//! names are sorted before printing. `io::ErrorKind` names come from the OS error by
//! `decode_error_kind` (ENOENT -> NotFound, EEXIST -> AlreadyExists).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};

fn main() -> io::Result<()> {
    let root = std::env::temp_dir().join(format!("blaze-ladder-s7-fs-{}", std::process::id()));
    if root.exists() {
        fs::remove_dir_all(&root)?;
    }
    fs::create_dir_all(root.join("sub/deeper"))?;
    fs::write(root.join("a.txt"), "alpha\nbeta\n")?;
    {
        let mut w = BufWriter::new(File::create(root.join("sub/b.txt"))?);
        for i in 0..3 {
            writeln!(w, "line {i}")?;
        }
    }
    let mut f = OpenOptions::new().append(true).open(root.join("a.txt"))?;
    f.write_all(b"gamma\n")?;
    drop(f);
    println!("{:?}", fs::read_to_string(root.join("a.txt"))?);
    println!("{}", fs::metadata(root.join("a.txt"))?.len());
    let lines: Vec<String> = BufReader::new(File::open(root.join("sub/b.txt"))?).lines().collect::<Result<_, _>>()?;
    println!("{:?}", lines);
    let mut names: Vec<String> = Vec::new();
    for e in fs::read_dir(&root)? {
        names.push(e?.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    println!("{:?}", names);
    let mut kinds = Vec::new();
    for e in fs::read_dir(root.join("sub"))? {
        let e = e?;
        let kind = if e.file_type()?.is_dir() { "dir" } else { "file" };
        kinds.push(format!("{}:{}", e.file_name().to_string_lossy(), kind));
    }
    kinds.sort();
    println!("{:?}", kinds);
    let mut f = File::open(root.join("a.txt"))?;
    f.seek(SeekFrom::Start(6))?;
    let mut buf = [0u8; 4];
    f.read_exact(&mut buf)?;
    println!("{}", String::from_utf8_lossy(&buf));
    fs::rename(root.join("a.txt"), root.join("renamed.txt"))?;
    println!("{} {}", root.join("a.txt").exists(), root.join("renamed.txt").is_file());
    fs::copy(root.join("renamed.txt"), root.join("sub/copy.txt"))?;
    println!("{}", fs::read(root.join("sub/copy.txt"))?.len());
    fs::remove_file(root.join("renamed.txt"))?;
    match File::open(root.join("renamed.txt")) {
        Err(e) => println!("{:?}", e.kind()),
        Ok(_) => println!("still there"),
    }
    println!("{:?}", fs::create_dir(root.join("sub")).map_err(|e| e.kind()));
    fs::remove_dir_all(&root)?;
    println!("{}", root.exists());
    Ok(())
}
