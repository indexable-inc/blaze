//! Build scripts that are neither compiled nor run, because what a run prints is a function of
//! its environment alone ([`blaze_build_script::inert`]; the idea is Relax's,
//! makepad `libs/relax/front/src/unitgraph_noop.rs`). When cargo asks rustc for such a script,
//! the wrapper writes, where the executable would be, the script's source under a line that
//! says what it is, and the dep-info rustc would have written; cargo runs it through
//! `blaze-engine build-script` as any other ([`super::script`]), and [`super::script_sandbox::run`]
//! works out what it prints in place of running anything. What it is given is what the sandbox
//! would have given it, so what it prints is what a run would have printed: no sandbox is
//! needed for a run of nothing.

use std::path::Path;

use blaze_build_script::inert::{self, Inert, Printed};

/// The first line of the file that stands for a script that is not compiled.
const HEAD: &str = "blaze: a build script that only prints, not compiled (inert 1)";

/// Whether the build script cargo asked rustc for with `args`, from `source`, is inert, and
/// if so writes what stands for it at `exe` and the dep-info rustc would have beside it.
/// `Err` is a file that could not be written.
pub fn elide(args: &[String], source: &Path, exe: &Path) -> Result<bool, String> {
    let edition = value(args, "--edition").unwrap_or("2015");
    let Ok(edition) = edition.parse() else { return Ok(false) };
    let Ok(text) = std::fs::read_to_string(source) else { return Ok(false) };
    if let Err(why) = inert::args(args).and_then(|()| inert::of(&text, edition).map(drop)) {
        if std::env::var_os("BLAZE_STATS").is_some() {
            eprintln!("blaze: the build script {} is compiled: {why}", source.display());
        }
        return Ok(false);
    }
    let write = |path: &Path, bytes: &[u8]| {
        let new = path.with_extension(format!("{}.new", std::process::id()));
        std::fs::write(&new, bytes)
            .and_then(|()| std::fs::rename(&new, path))
            .map_err(|it| format!("writing {}: {it}", path.display()))
    };
    write(exe, format!("{HEAD}\n{edition}\n{text}").as_bytes())?;
    let target = crate::dep_info::escaped(&exe.display().to_string());
    let file = crate::dep_info::escaped(&source.display().to_string());
    write(&exe.with_extension("d"), format!("{target}: {file}\n\n{file}:\n").as_bytes())?;
    if std::env::var_os("BLAZE_STATS").is_some() {
        eprintln!("blaze: the build script {} only prints, and is not compiled", source.display());
    }
    Ok(true)
}

/// The inert script that stands at `real`, when one does. A file that says it is one and
/// whose script is not is an error.
pub fn read(real: &Path) -> anyhow::Result<Option<Inert>> {
    use std::io::Read as _;
    // An executable is not read whole to tell it is not one.
    let Ok(mut file) = std::fs::File::open(real) else { return Ok(None) };
    let mut head = vec![0; HEAD.len() + 1];
    if file.read_exact(&mut head).is_err() || head[..HEAD.len()] != *HEAD.as_bytes() {
        return Ok(None);
    }
    if head[HEAD.len()] != b'\n' {
        return Ok(None);
    }
    let mut rest = String::new();
    file.read_to_string(&mut rest)
        .map_err(|it| anyhow::anyhow!("reading {}: {it}", real.display()))?;
    let (edition, source) = rest.split_once('\n').unwrap_or((&rest, ""));
    let edition = edition.parse().map_err(|it| anyhow::anyhow!("{} names {it}", real.display()))?;
    let script = inert::of(source, edition)
        .map_err(|why| anyhow::anyhow!("{} is no inert build script: {why}", real.display()))?;
    Ok(Some(script))
}

/// What `script` prints run with `env`, as a process that ran and succeeded.
pub fn run(script: &Inert, env: &[(String, String)]) -> std::process::Output {
    // The last of a name is what a process started with them has.
    let Printed { stdout, stderr } =
        script.run(|name| env.iter().rev().find(|it| it.0 == name).map(|it| it.1.as_str()));
    std::process::Output { status: success(), stdout, stderr }
}

#[cfg(unix)]
fn success() -> std::process::ExitStatus {
    std::os::unix::process::ExitStatusExt::from_raw(0)
}

#[cfg(windows)]
fn success() -> std::process::ExitStatus {
    std::os::windows::process::ExitStatusExt::from_raw(0)
}

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let joined = format!("{flag}=");
    let at = args.iter().position(|it| it == flag || it.starts_with(&joined))?;
    match args[at].strip_prefix(&joined) {
        Some(value) => Some(value),
        None => args.get(at + 1).map(String::as_str),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn stands_for_a_script_and_runs_as_one() {
        let dir = std::env::temp_dir().join(format!("blaze-inert-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("build.rs");
        std::fs::write(
            &source,
            "fn main() {\n    println!(\"cargo:rerun-if-changed=build.rs\");\n    if \
             std::env::var(\"X\").is_ok() { println!(\"cargo:rustc-cfg=x\") }\n}\n",
        )
        .unwrap();
        let exe = dir.join("build_script_build-0123");
        let args: Vec<String> =
            ["--crate-name", "build_script_build", "--edition=2021", "build.rs", "--crate-type", "bin"]
                .map(str::to_owned)
                .into();
        assert_eq!(elide(&args, &source, &exe), Ok(true));
        let dep_info = std::fs::read_to_string(dir.join("build_script_build-0123.d")).unwrap();
        let (exe_text, source_text) = (exe.display(), source.display());
        assert_eq!(dep_info, format!("{exe_text}: {source_text}\n\n{source_text}:\n"));
        let script = read(&exe).unwrap().unwrap();
        let env = [("X".to_owned(), "1".to_owned())];
        let ran = run(&script, &env);
        assert!(ran.status.success());
        assert_eq!(ran.stdout, b"cargo:rerun-if-changed=build.rs\ncargo:rustc-cfg=x\n");
        assert_eq!(run(&script, &[]).stdout, b"cargo:rerun-if-changed=build.rs\n");

        std::fs::write(&source, "fn main() { cc::Build::new().compile(\"x\"); }").unwrap();
        let other = dir.join("build_script_build-4567");
        assert_eq!(elide(&args, &source, &other), Ok(false));
        assert!(!other.exists());
        std::fs::write(&other, b"\x7fELF").unwrap();
        assert!(read(&other).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
