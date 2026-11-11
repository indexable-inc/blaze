//! `blaze tree`: its command line read as cargo 1.101 reads `cargo tree`'s, the tree made and
//! printed by blaze-tree over blaze-metadata's resolve of the workspace. It runs in this
//! process and asks nothing of the daemon; the lock files are decided as a build's are
//! (docs/cargo.md, "Lock files").

use std::{alloc::Global, ffi::OsString, io::IsTerminal as _, path::PathBuf};

use blaze_core::exit::Exit;
use blaze_hash::HashSet;
use blaze_tree::{
    DepKind, DisplayDepth, EdgeKind, Kinds, Options, Packages, Platformed, Prefix, Target,
};
use blaze_units::stdlib::target::{CfgArgs, target_cfgs};

use super::{
    own::{self, Read, switch, valued},
    say,
};

const USAGE: &str = "blaze tree [OPTIONS]";

const EDGES: &[&str] = &[
    "all",
    "normal",
    "build",
    "dev",
    "features",
    "public",
    "no-normal",
    "no-build",
    "no-dev",
    "no-proc-macro",
];

/// What the options are read against besides the command line: the terminal and the
/// environment, which decide the charset and the color when no flag does.
#[derive(Clone, Copy, Debug, Default)]
struct Shown {
    /// Standard output is a terminal.
    terminal: bool,
    /// The locale says UTF-8 ([`utf8_locale`]).
    utf8: bool,
    /// `CARGO_TERM_COLOR` says `always` (`Some(true)`) or `never`.
    term_color: Option<bool>,
    /// `NO_COLOR` is set.
    no_color: bool,
}

impl Shown {
    fn of_process() -> Shown {
        Shown {
            terminal: std::io::stdout().is_terminal(),
            utf8: utf8_locale(),
            term_color: match std::env::var("CARGO_TERM_COLOR").as_deref() {
                Ok("always") => Some(true),
                Ok("never") => Some(false),
                _ => None,
            },
            no_color: std::env::var_os("NO_COLOR").is_some(),
        }
    }
}

/// `blaze tree WORDS`: its exit code.
pub fn main(words: Vec<OsString>, color: bool) -> i32 {
    let flags = [
        switch(&["-a", "--all"]),
        switch(&["--no-dev-dependencies"]),
        valued(&["-e", "--edges"]),
        valued(&["-i", "--invert"]),
        valued(&["--prune"]),
        valued(&["--depth"]),
        switch(&["--no-indent"]),
        switch(&["--prefix-depth"]),
        valued(&["--prefix"]),
        switch(&["--no-dedupe"]),
        switch(&["-d", "--duplicates", "--duplicate"]),
        valued(&["--charset"]),
        valued(&["-f", "--format"]),
        switch(&["-V", "--version"]),
        valued(&["-p", "--package"]),
        switch(&["--workspace"]),
        valued(&["--exclude"]),
        valued(&["-F", "--features"]),
        switch(&["--all-features"]),
        switch(&["--no-default-features"]),
        switch(&["--all-targets"]),
        valued(&["--target"]),
        valued(&["--manifest-path"]),
    ];
    let read = match own::read_or_refuse(words, &flags, USAGE, 0, color) {
        Ok(read) => read,
        Err(code) => return code,
    };
    if read.has("--version") {
        println!("{}", crate::version());
        return 0;
    }
    let mut warn = |what: &str| eprint!("{}", say::warning(what, color));
    let opts = match options(&read, Shown::of_process(), &mut warn) {
        Ok(opts) => opts,
        Err(Refused { why, usage: true }) => {
            eprint!("{}", say::error(&why, color));
            eprintln!("\nUsage: {USAGE}\n\nFor more information, try '--help'.");
            return Exit::Usage.code();
        }
        Err(Refused { why, usage: false }) => {
            eprint!("{}", say::error(&why, color));
            return Exit::Failed.code();
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprint!("{}", say::error(&format!("blaze tree: the current directory: {error}"), color));
            return Exit::Failed.code();
        }
    };
    let printed = || -> anyhow::Result<String> {
        let targets = match &opts.target {
            Target::Specific(triples) => triples.clone(), // clone: the `--target`s, once
            Target::Host | Target::All => Vec::new(),
        };
        let platformed = |triple: String| -> anyhow::Result<Platformed> {
            let cfg = target_cfgs(&triple, &CfgArgs::default()).map_err(anyhow::Error::msg)?;
            Ok(Platformed { cfg, triple })
        };
        let host = crate::toolchain::host().ok_or_else(|| {
            anyhow::anyhow!(
                "blaze tree: blaze has no toolchain for this host ({} {}), so it cannot say which of the workspace's dependencies the host builds",
                std::env::consts::ARCH,
                std::env::consts::OS
            )
        })?;
        let kinds = Kinds {
            host: platformed(host.to_owned())?,
            targets: targets.into_iter().map(platformed).collect::<anyhow::Result<_>>()?,
        };
        let env = own::env();
        let manifest_path = read.value("--manifest-path").map(PathBuf::from);
        let config = read.values("--config");
        let features = read.features();
        let ignore_pin_for = blaze_units::query::env_var(&env, "BLAZE_IGNORE_PIN_FOR")
            .filter(|it| !it.is_empty())
            .map(str::to_owned);
        let lock = blaze_units::lock::Inputs {
            ask: own::lock_ask(&read, &env).map_err(anyhow::Error::msg)?,
            running: own::running(),
            ignore_pin_for: ignore_pin_for.as_deref(),
            foreign: false,
        };
        let asked = blaze_metadata::Asked {
            cwd: &cwd,
            env,
            manifest_path: manifest_path.as_deref(),
            config: &config,
            features: &features,
            all_features: read.has("--all-features"),
            no_default_features: read.has("--no-default-features"),
            build: None,
            platform: None,
            offline: read.has("--offline") || read.has("--frozen"),
            rustc_version: Some(crate::toolchain::ours_release()),
            lock,
        };
        let mut out = String::new(); // alloc: the tree, printed once
        blaze_tree::print_tree(&asked, &opts, &kinds, &mut out, &mut |it: &str| {
            eprint!("{}", say::warning(it, color))
        })?;
        Ok(out)
    };
    match printed() {
        Ok(out) => {
            print!("{out}");
            0
        }
        Err(error) => own::failed(&error, &cwd, color),
    }
}

/// A command line cargo refuses, and why: with its usage, as clap refuses it, or as a command
/// that failed.
#[derive(Debug)]
struct Refused {
    why: String,
    usage: bool,
}

fn usage(why: String) -> Refused {
    Refused { why, usage: true }
}

fn failure(why: &str) -> Refused {
    Refused { why: why.to_owned(), usage: false }
}

fn invalid(value: &str, flag: &str, possible: &[&str]) -> Refused {
    usage(format!(
        "invalid value '{value}' for '{flag}'\n  [possible values: {}]",
        possible.join(", ")
    ))
}

/// cargo's `commands/tree.rs`: the flags as [`Options`], each flag cargo renamed warned of.
fn options(
    read: &Read,
    shown: Shown,
    // dyn: the warnings of a command line, a few at most.
    warn: &mut dyn FnMut(&str),
) -> Result<Options, Refused> {
    let prefix = if read.has("--no-indent") {
        warn("the --no-indent flag has been changed to --prefix=none");
        "none".to_owned()
    } else if read.has("--prefix-depth") {
        warn("the --prefix-depth flag has been changed to --prefix=depth");
        "depth".to_owned()
    } else {
        read.value("--prefix").unwrap_or_else(|| "indent".to_owned())
    };
    let prefix = match prefix.as_str() {
        "none" => Prefix::None,
        "indent" => Prefix::Indent,
        "depth" => Prefix::Depth,
        other => return Err(invalid(other, "--prefix <PREFIX>", &["depth", "indent", "none"])),
    };
    let no_dedupe = read.has("--no-dedupe") || read.has("--all");
    if read.has("--all") {
        warn(
            "the `cargo tree` --all flag has been changed to --no-dedupe, and may be removed in a future version\n  |\n  = help: if you are looking to display all workspace members, use the --workspace flag",
        );
    }
    let targets = if read.has("--all-targets") {
        warn("the --all-targets flag has been changed to --target=all");
        vec!["all".to_owned()]
    } else {
        read.values("--target")
    };
    let (edge_kinds, no_proc_macro, public) = edge_kinds(read, warn)?;
    let graph_features = edge_kinds.contains(&EdgeKind::Feature);
    let display_depth = match read.value("--depth") {
        None => DisplayDepth::MaxDisplayDepth(u32::MAX),
        Some(depth) if depth == "workspace" => DisplayDepth::Workspace,
        Some(depth) => DisplayDepth::MaxDisplayDepth(depth.parse().map_err(|_| {
            usage(format!(
                "invalid value '{depth}' for '--depth <DEPTH>': supported values for --depth are non-negative integers and `workspace`, but `{depth}` is unknown"
            ))
        })?),
    };
    let package = read.values("--package");
    let exclude = read.values("--exclude");
    let packages = match (read.has("--workspace"), exclude.is_empty(), package.is_empty()) {
        (false, true, true) => Packages::Default,
        (false, true, false) => Packages::Packages(package),
        (false, false, _) => {
            return Err(failure("--exclude can only be used together with --workspace"));
        }
        (true, true, _) => Packages::All(package),
        (true, false, _) => Packages::OptOut(exclude),
    };
    let unicode = match read.value("--charset").as_deref() {
        None => !shown.terminal || shown.utf8,
        Some("utf8") => true,
        Some("ascii") => false,
        Some(other) => return Err(invalid(other, "--charset <CHARSET>", &["utf8", "ascii"])),
    };
    let color = match read.value("--color").as_deref() {
        Some("always") => true,
        Some("never") => false,
        _ => match shown.term_color {
            Some(color) => color,
            None => shown.terminal && !shown.no_color,
        },
    };
    let opts = Options {
        packages,
        target: Target::from_cli(targets),
        edge_kinds,
        invert: read.values("--invert"),
        pkgs_to_prune: read.values("--prune"),
        prefix,
        no_dedupe,
        duplicates: read.has("--duplicates"),
        format: read.value("--format").unwrap_or_else(|| "{p}".to_owned()),
        graph_features,
        display_depth,
        no_proc_macro,
        public,
        unicode,
        color,
    };
    if opts.graph_features && opts.duplicates {
        return Err(failure("the `-e features` flag does not support `--duplicates`"));
    }
    Ok(opts)
}

/// Whether the locale says UTF-8, as the `supports-unicode` crate cargo reads judges it.
fn utf8_locale() -> bool {
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|it| std::env::var(it).ok().filter(|it| !it.is_empty()))
        .unwrap_or_default()
        .to_ascii_lowercase();
    std::env::var("TERM").as_deref() != Ok("linux")
        && (locale.contains("utf-8") || locale.contains("utf8"))
}

/// `--edges`: cargo's `parse_edge_kinds`. The kinds, whether proc macros are left out, and
/// whether only public dependencies are shown.
fn edge_kinds(
    read: &Read,
    // dyn: the warnings of a command line, a few at most.
    warn: &mut dyn FnMut(&str),
) -> Result<(HashSet<EdgeKind, Global>, bool, bool), Refused> {
    let given = read.values("--edges");
    let mut kinds: Vec<&str> = given.iter().flat_map(|it| it.split(',')).collect();
    if let Some(other) = kinds.iter().find(|it| !EDGES.contains(it)) {
        return Err(invalid(other, "--edges <KINDS>", EDGES));
    }
    let no_proc_macro = kinds.contains(&"no-proc-macro");
    let public = kinds.contains(&"public");
    kinds.retain(|it| !matches!(*it, "no-proc-macro" | "public"));
    if read.has("--no-dev-dependencies") {
        warn("the --no-dev-dependencies flag has changed to -e=no-dev");
        kinds.push("no-dev");
    }
    if kinds.is_empty() {
        kinds.extend(["normal", "build", "dev"]);
    }
    if public {
        return Err(failure("`--edges public` requires `-Zunstable-options`"));
    }
    // alloc: the edge kinds, four at most
    let mut result: HashSet<EdgeKind, Global> = HashSet::new_in(Global);
    let defaults = [
        EdgeKind::Dep(DepKind::Normal),
        EdgeKind::Dep(DepKind::Build),
        EdgeKind::Dep(DepKind::Development),
    ];
    if kinds.iter().any(|k| k.starts_with("no-")) {
        result.extend(defaults);
        for kind in &kinds {
            match *kind {
                "no-normal" => result.remove(&EdgeKind::Dep(DepKind::Normal)),
                "no-build" => result.remove(&EdgeKind::Dep(DepKind::Build)),
                "no-dev" => result.remove(&EdgeKind::Dep(DepKind::Development)),
                "features" => result.insert(EdgeKind::Feature),
                _ => {
                    return Err(failure(&format!(
                        "`{kind}` dependency kind cannot be mixed with \"no-normal\", \"no-build\", or \"no-dev\" dependency kinds"
                    )));
                }
            };
        }
        return Ok((result, no_proc_macro, public));
    }
    for kind in &kinds {
        match *kind {
            "all" => {
                result.extend(defaults);
                result.insert(EdgeKind::Feature);
            }
            "features" => {
                result.insert(EdgeKind::Feature);
            }
            "normal" => {
                result.insert(EdgeKind::Dep(DepKind::Normal));
            }
            "build" => {
                result.insert(EdgeKind::Dep(DepKind::Build));
            }
            _ => {
                result.insert(EdgeKind::Dep(DepKind::Development));
            }
        }
    }
    if kinds == ["features"] {
        result.extend(defaults);
    }
    Ok((result, no_proc_macro, public))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAGS: [own::Flag; 10] = [
        switch(&["-a", "--all"]),
        switch(&["--no-dev-dependencies"]),
        valued(&["-e", "--edges"]),
        valued(&["-i", "--invert"]),
        valued(&["--depth"]),
        valued(&["--prefix"]),
        switch(&["-d", "--duplicates", "--duplicate"]),
        valued(&["--charset"]),
        valued(&["-p", "--package"]),
        switch(&["--workspace"]),
    ];

    fn asked(words: &[&str]) -> (Result<Options, Refused>, Vec<String>) {
        let read = own::read(words.iter().map(OsString::from).collect(), &FLAGS).unwrap();
        let mut warned = Vec::new();
        let opts = options(&read, Shown::default(), &mut |it: &str| warned.push(it.to_owned()));
        (opts, warned)
    }

    const NORMAL: EdgeKind = EdgeKind::Dep(DepKind::Normal);
    const BUILD: EdgeKind = EdgeKind::Dep(DepKind::Build);
    const DEV: EdgeKind = EdgeKind::Dep(DepKind::Development);

    #[test]
    fn with_no_flags_the_tree_is_cargos_default() {
        let (opts, warned) = asked(&[]);
        let opts = opts.unwrap();
        assert!(warned.is_empty(), "{warned:?}");
        assert!(matches!(opts.packages, Packages::Default));
        assert!(opts.target == Target::Host);
        assert!([NORMAL, BUILD, DEV].iter().all(|it| opts.edge_kinds.contains(it)));
        assert!(!opts.edge_kinds.contains(&EdgeKind::Feature) && !opts.graph_features);
        assert!(matches!(opts.prefix, Prefix::Indent));
        assert!(matches!(opts.display_depth, DisplayDepth::MaxDisplayDepth(u32::MAX)));
        assert_eq!(opts.format, "{p}");
        // No terminal: cargo writes UTF-8 lines and no color into a pipe.
        assert!(opts.unicode && !opts.color);
    }

    #[test]
    fn the_flags_are_read_as_cargo_reads_them() {
        let (opts, warned) = asked(&[
            "-e",
            "no-dev,features",
            "-i",
            "serde",
            "--depth",
            "2",
            "--prefix",
            "none",
            "--charset",
            "ascii",
            "--workspace",
            "-p",
            "app",
            "--color",
            "always",
        ]);
        let opts = opts.unwrap();
        assert!(warned.is_empty(), "{warned:?}");
        assert!(opts.edge_kinds.contains(&NORMAL) && opts.edge_kinds.contains(&BUILD));
        assert!(!opts.edge_kinds.contains(&DEV), "`no-dev` takes dev dependencies out");
        assert!(opts.edge_kinds.contains(&EdgeKind::Feature) && opts.graph_features);
        assert_eq!(opts.invert, ["serde"]);
        assert!(matches!(opts.display_depth, DisplayDepth::MaxDisplayDepth(2)));
        assert!(matches!(opts.prefix, Prefix::None));
        assert!(matches!(&opts.packages, Packages::All(named) if named == &["app"]));
        assert!(!opts.unicode && opts.color);

        let (opts, warned) = asked(&["--all", "--no-dev-dependencies", "--depth", "workspace"]);
        let opts = opts.unwrap();
        assert_eq!(warned.len(), 2, "each renamed flag is warned of: {warned:?}");
        assert!(opts.no_dedupe && !opts.edge_kinds.contains(&DEV));
        assert!(matches!(opts.display_depth, DisplayDepth::Workspace));
    }

    #[test]
    fn what_cargo_refuses_is_refused_as_cargo_says_it() {
        let refused = |words: &[&str]| asked(words).0.err().unwrap();
        let prefix = refused(&["--prefix", "tabs"]);
        assert!(prefix.usage && prefix.why.contains("'--prefix <PREFIX>'"), "{prefix:?}");
        let edges = refused(&["-e", "sideways"]);
        assert!(edges.usage && edges.why.contains("'sideways'"), "{edges:?}");
        let depth = refused(&["--depth", "deep"]);
        assert!(depth.usage && depth.why.contains("`deep` is unknown"), "{depth:?}");
        let mixed = refused(&["-e", "no-dev,normal"]);
        assert!(!mixed.usage && mixed.why.contains("cannot be mixed"), "{mixed:?}");
        let public = refused(&["-e", "public"]);
        assert!(!public.usage && public.why.contains("-Zunstable-options"), "{public:?}");
        let duplicates = refused(&["-e", "features", "-d"]);
        assert!(duplicates.why.contains("--duplicates"), "{duplicates:?}");
    }
}
