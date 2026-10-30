//! Cargo's home directory, and what `blaze enable` left in it when plain cargo still built
//! through blaze: `build.rustc-workspace-wrapper` naming `blaze-rustc` in cargo's
//! configuration, and `cargo fmt` and `cargo clippy` linked to `blaze` in its `bin`, with
//! rustup's proxies kept in `blaze-kept`, all of it recorded in `blaze-enabled.json`.
//! [`undo_enable`] puts that back as it was.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Deserialize;
use toml::{Table, Value};

/// The table and the key of cargo's configuration that `enable` named the wrapper in.
const TABLE: &str = "build";
const KEY: &str = "rustc-workspace-wrapper";

/// Cargo's home directory, as cargo finds it.
pub fn cargo_home() -> Option<PathBuf> {
    let set = std::env::var_os("CARGO_HOME").filter(|it| !it.is_empty()).map(PathBuf::from);
    let home = || std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    set.or_else(|| Some(Path::new(&home()?).join(".cargo")))
}

/// Where cargo keeps the package in `dir` when it is a registry's or a git dependency's: the
/// cargo home above its `registry/src` or `git/checkouts`.
pub fn cargo_home_of(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find_map(|it| {
        let parent = it.parent()?;
        let name = it.file_name()?;
        let below = (name == "src" && parent.file_name()? == "registry")
            || (name == "checkouts" && parent.file_name()? == "git");
        below.then(|| parent.parent().map(Path::to_owned)).flatten()
    })
}

/// What `enable` recorded of what it changed.
#[derive(Deserialize)]
struct Enabled {
    /// The configuration file it wrote.
    config: PathBuf,
    /// The file as it was, when there was one.
    before: Option<String>,
    /// The file as `enable` wrote it.
    written: String,
    /// The wrapper it named there.
    wrapper: PathBuf,
    #[serde(default)]
    links: Vec<Link>,
}

/// A link `enable` made.
#[derive(Deserialize)]
struct Link {
    /// `cargo-<command>` in cargo's home.
    path: PathBuf,
    /// Where the file that was there is kept, when there was one.
    kept: Option<PathBuf>,
    /// Whether `enable` made the directory the link is in.
    #[serde(default)]
    made_dir: bool,
}

/// Puts back what an earlier `blaze enable` changed in cargo's home and says so in one
/// line. With no record of one, this is one file found missing.
pub fn undo_enable() -> anyhow::Result<()> {
    let Some(record) = cargo_home().map(|it| it.join("blaze-enabled.json")) else {
        return Ok(());
    };
    let text = match std::fs::read_to_string(&record) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", record.display())),
    };
    let enabled: Enabled = serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is `blaze enable`'s record and blaze can't read it; remove it, and `{TABLE}.{KEY}` \
             from cargo's configuration",
            record.display()
        )
    })?;
    let mut undone = Vec::new();
    if config_undone(&enabled)? {
        undone.push(format!("`{TABLE}.{KEY}` from {}", enabled.config.display()));
    }
    let commands = links_undone(&enabled.links)?;
    if !commands.is_empty() {
        undone.push(format!("the links of `{}`", commands.join("` and `")));
    }
    std::fs::remove_file(&record).with_context(|| format!("removing {}", record.display()))?;
    let undone = match undone.is_empty() {
        true => format!("its record {}", record.display()),
        false => undone.join(" and "),
    };
    crate::cargo::say::note(&format!(
        "plain cargo no longer builds through blaze: removed what `blaze enable` wrote, {undone}"
    ));
    Ok(())
}

/// Puts cargo's configuration back as it was before `enable`: a file nobody wrote since is
/// the same file again, byte for byte, and in one that was edited the one key is put back,
/// when it still names blaze's wrapper. Whether the file changed.
fn config_undone(enabled: &Enabled) -> anyhow::Result<bool> {
    let config = &enabled.config;
    let Ok(now) = std::fs::read_to_string(config) else { return Ok(false) };
    match &enabled.before {
        None if now == enabled.written => std::fs::remove_file(config)?,
        Some(before) if now == enabled.written => std::fs::write(config, before)?,
        _ => {
            let ours = enabled.wrapper.to_str();
            if parsed(&now).ok().and_then(|it| wrapper_of(&it)).as_deref() != ours {
                return Ok(false);
            }
            let before = enabled.before.as_deref().unwrap_or_default();
            let line = parsed(before).ok().and_then(|_| wrapper_line(before).map(|it| it.1));
            let text = without_wrapper(&now, line.as_deref()).with_context(|| {
                format!(
                    "{} names blaze's wrapper `{}` in a way blaze does not edit; remove \
                     `{TABLE}.{KEY}` from it",
                    config.display(),
                    enabled.wrapper.display()
                )
            })?;
            std::fs::write(config, text)?;
        }
    }
    Ok(true)
}

/// Whether `path` is a link to a program named `name`.
fn links_to(path: &Path, name: &str) -> bool {
    std::fs::read_link(path).is_ok_and(|it| it.file_stem().is_some_and(|it| it == name))
}

/// Whether `path` is a link to a `blaze`, or to the program `enable` linked before `blaze`
/// was the only command.
fn ours(path: &Path) -> bool {
    links_to(path, "blaze") || links_to(path, crate::renamed::OLD_LAUNCHER)
}

/// Puts back what `enable` linked. A link replaced since, as `rustup update` puts its proxy
/// back, is left as it is, and the file kept for it goes. The commands whose link went.
fn links_undone(links: &[Link]) -> anyhow::Result<Vec<String>> {
    let mut undone = Vec::new();
    for Link { path, kept, .. } in links {
        if ours(path) {
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
            if let Some(kept) = kept {
                std::fs::rename(kept, path)
                    .with_context(|| format!("moving {} back", kept.display()))?;
            }
            undone.push(command(path));
        } else if let Some(kept) = kept {
            if std::fs::symlink_metadata(path).is_ok() {
                std::fs::remove_file(kept)
                    .with_context(|| format!("removing {}", kept.display()))?;
            } else {
                std::fs::rename(kept, path)
                    .with_context(|| format!("moving {} back", kept.display()))?;
            }
        }
    }
    if let Some(dir) = links.iter().find_map(|it| it.kept.as_deref()?.parent()) {
        let _ = std::fs::remove_dir(dir);
    }
    // The directory `enable` made for its links goes too, when nothing else was put in it.
    if let Some(dir) = links.iter().find(|it| it.made_dir).and_then(|it| it.path.parent()) {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(undone)
}

/// The command `cargo-<command>` at `path` is, as typed: `cargo fmt`.
fn command(path: &Path) -> String {
    let stem = path.file_stem().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
    format!("cargo {}", stem.strip_prefix("cargo-").unwrap_or(&stem))
}

fn parsed(text: &str) -> Result<Table, toml::de::Error> {
    text.parse::<Table>()
}

fn wrapper_of(table: &Table) -> Option<String> {
    table.get(TABLE)?.get(KEY)?.as_str().map(str::to_owned)
}

/// Where the key is set in `text`: the number of the line, and the line. It is looked for
/// as `rustc-workspace-wrapper = ` under `[build]` and as `build.rustc-workspace-wrapper = `
/// above the first table, which is how `enable` wrote it and how it is written by hand.
fn wrapper_line(text: &str) -> Option<(usize, String)> {
    let sets = |line: &str, key: &str| {
        line.trim_start().strip_prefix(key).is_some_and(|rest| rest.trim_start().starts_with('='))
    };
    let mut table = None;
    for (at, line) in text.lines().enumerate() {
        if let Some(header) = header(line) {
            table = Some(header);
            continue;
        }
        let found = match table {
            None => sets(line, &format!("{TABLE}.{KEY}")),
            Some(table) => table == TABLE && sets(line, KEY),
        };
        if found {
            return Some((at, line.to_owned()));
        }
    }
    None
}

/// The name of the table the line starts, when it starts one.
fn header(line: &str) -> Option<&str> {
    let line = line.trim();
    let line = line.split_once('#').map_or(line, |(before, _)| before.trim_end());
    Some(line.strip_prefix('[')?.strip_suffix(']')?.trim())
}

/// `text` with the line that sets the key replaced by `line`, the one that was there
/// before `enable`, or without it. The file ends its lines as it did. `None` when what is
/// left is not the file but for the key.
fn without_wrapper(text: &str, line: Option<&str>) -> Option<String> {
    let Some((at, _)) = wrapper_line(text) else { return Some(text.to_owned()) };
    let ending = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<&str> = text.lines().collect();
    match line {
        Some(line) => lines[at] = line,
        None => _ = lines.remove(at),
    }
    let written: String = lines.iter().flat_map(|it| [it, ending]).collect();
    let mut expected = parsed(text).ok()?;
    let was = line.and_then(|_| wrapper_of(&parsed(&written).ok()?));
    let table = expected.get_mut(TABLE)?.as_table_mut()?;
    match was {
        Some(was) => table.insert(KEY.to_owned(), Value::String(was)),
        None => table.remove(KEY),
    };
    let mut got = parsed(&written).ok()?;
    // A dotted `build.` key alone leaves no `[build]` table behind it.
    for it in [&mut expected, &mut got] {
        if it.get(TABLE).and_then(Value::as_table).is_some_and(Table::is_empty) {
            it.remove(TABLE);
        }
    }
    (got == expected).then_some(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: &str = "/opt/blaze/blaze-rustc";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-home-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        dir
    }

    fn enabled(config: &Path, before: Option<&str>, written: &str) -> Enabled {
        Enabled {
            config: config.to_owned(),
            before: before.map(str::to_owned),
            written: written.to_owned(),
            wrapper: PathBuf::from(OURS),
            links: Vec::new(),
        }
    }

    #[test]
    fn a_file_nobody_wrote_since_is_the_file_before() {
        let home = scratch("unwritten");
        let config = home.join("config.toml");
        let written = "[net]\noffline = true\n\n[build]\nrustc-workspace-wrapper = \"/opt/blaze/blaze-rustc\"\n";
        std::fs::write(&config, written).unwrap();
        assert!(config_undone(&enabled(&config, Some("[net]\noffline = true"), written)).unwrap());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "[net]\noffline = true");
        std::fs::write(&config, written).unwrap();
        assert!(config_undone(&enabled(&config, None, written)).unwrap());
        assert!(!config.exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn an_edited_file_loses_the_key_alone() {
        let home = scratch("edited");
        let config = home.join("config.toml");
        let now = "# mine\n[build] # all\nrustc-workspace-wrapper = \"/opt/blaze/blaze-rustc\"\njobs = 4\n";
        std::fs::write(&config, now).unwrap();
        assert!(config_undone(&enabled(&config, None, "")).unwrap());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "# mine\n[build] # all\njobs = 4\n");
        // Another wrapper named since is the user's.
        let theirs = "[build]\nrustc-workspace-wrapper = \"theirs\"\n";
        std::fs::write(&config, theirs).unwrap();
        assert!(!config_undone(&enabled(&config, None, "")).unwrap());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), theirs);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn the_wrapper_that_was_there_comes_back() {
        let before = "[build]\r\nrustc-workspace-wrapper = 'mine' # kept\r\n";
        let (_, line) = wrapper_line(before).unwrap();
        let written = "[build]\r\nrustc-workspace-wrapper = \"/opt/blaze/blaze-rustc\"\r\n";
        assert_eq!(without_wrapper(written, Some(&line)).unwrap(), before);
        let dotted =
            "build.rustc-workspace-wrapper = \"/opt/blaze/blaze-rustc\"\n[net]\noffline = true\n";
        assert_eq!(without_wrapper(dotted, None).unwrap(), "[net]\noffline = true\n");
    }

    #[test]
    fn the_same_key_of_another_table_is_not_the_key() {
        let text = "[target.x86_64-unknown-linux-gnu]\nrustc-workspace-wrapper = \"other\"\n";
        assert_eq!(wrapper_line(text), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_proxy_comes_back_and_a_link_replaced_since_stays() {
        let home = scratch("links");
        let fmt = home.join("bin/cargo-fmt");
        let clippy = home.join("bin/cargo-clippy");
        let kept = home.join("blaze-kept");
        std::fs::create_dir_all(&kept).unwrap();
        std::fs::write(kept.join("cargo-fmt"), "rustup's proxy").unwrap();
        std::fs::write(kept.join("cargo-clippy"), "old proxy").unwrap();
        std::os::unix::fs::symlink(home.join("blaze"), &fmt).unwrap();
        std::fs::write(&clippy, "new proxy").unwrap();
        let links = [
            Link { path: fmt.clone(), kept: Some(kept.join("cargo-fmt")), made_dir: false },
            Link { path: clippy.clone(), kept: Some(kept.join("cargo-clippy")), made_dir: false },
        ];
        assert_eq!(links_undone(&links).unwrap(), ["cargo fmt"]);
        assert_eq!(std::fs::read_to_string(&fmt).unwrap(), "rustup's proxy");
        assert_eq!(std::fs::read_to_string(&clippy).unwrap(), "new proxy");
        assert!(!kept.exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_record_of_enable_is_read() {
        let record = r#"{"config": "/h/config.toml", "before": null, "written": "x",
            "chained": null, "wrapper": "/opt/blaze/blaze-rustc",
            "links": [{"path": "/h/bin/cargo-fmt", "kept": null}]}"#;
        let enabled: Enabled = serde_json::from_str(record).unwrap();
        assert_eq!(enabled.links.len(), 1);
        assert_eq!(enabled.wrapper, Path::new(OURS));
    }
}
