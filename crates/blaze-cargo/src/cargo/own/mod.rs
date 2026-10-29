//! Every `blaze` subcommand that is not a build: blaze's own, with no cargo, rustc,
//! rustdoc, clippy-driver or rustfmt started. One blaze has not written yet fails, saying so;
//! none is handed to cargo, nor is a cargo plugin run. `docs/cargo.md`, "Every subcommand",
//! is the table of what each one is.

pub mod args;
pub mod clean;
pub mod clippy;
pub mod config;
pub mod fmt;
pub mod manifest;
pub mod new;
pub mod registry;
pub mod remove;
pub mod report;
pub mod tree;
pub mod uninstall;
pub mod vendor;

use args::Args;

/// Where the table of every subcommand is, for the line under a refusal.
const TABLE: &str =
    "https://github.com/indexable-inc/blaze/blob/main/docs/cargo.md#every-subcommand";

/// What blaze has of a subcommand of cargo's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// A build, run, test or check, which `cargo/mod.rs` answers.
    Build,
    /// blaze's own.
    Own,
    /// Not written yet, with what it waits for.
    Not(&'static str),
}

/// A subcommand of cargo's, with the names cargo also takes for it.
pub struct Subcommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub status: Status,
    pub about: &'static str,
}

const fn sub(name: &'static str, status: Status, about: &'static str) -> Subcommand {
    Subcommand { name, aliases: &[], status, about }
}

const fn aka(
    name: &'static str,
    aliases: &'static [&'static str],
    status: Status,
    about: &'static str,
) -> Subcommand {
    Subcommand { name, aliases, status, about }
}

/// A build blaze does not do yet: the build path's (docs/no-rustc.md).
const A_BUILD: Status = Status::Not("a build blaze does not do yet");
/// What needs blaze's resolver to choose versions, not only to follow `Cargo.lock` (#321).
const CHOOSES_VERSIONS: Status =
    Status::Not("it chooses versions, which blaze's resolver does not yet (#321)");
/// What talks to a registry's web API, which blaze has no client of yet.
const PUBLISH: Status = Status::Not("it packages the crate first, a build blaze does not do yet");
const INFO: Status = Status::Not("blaze's `cargo info` is not written yet");

/// Every subcommand of the pinned nightly's cargo, and `fmt` and `clippy`, which rustup ships
/// beside it.
pub const SUBCOMMANDS: &[Subcommand] = &[
    aka("build", &["b"], Status::Build, "compile the workspace's programs"),
    aka("check", &["c"], Status::Build, "what a build would say of the source, building nothing"),
    aka("run", &["r"], Status::Build, "build and run a program"),
    aka("test", &["t"], Status::Build, "build and run the tests"),
    sub("fmt", Status::Own, "format the source"),
    sub("clippy", Status::Own, "lint the source with clippy's lints, blaze's own"),
    aka("doc", &["d"], Status::Own, "write the documentation"),
    sub("metadata", Status::Own, "the workspace and its resolved dependencies, as JSON"),
    sub("locate-project", Status::Own, "the manifest of the package or workspace"),
    sub("verify-project", Status::Own, "whether the manifest reads"),
    sub("version", Status::Own, "blaze's version"),
    sub("help", Status::Own, "this list, or what a command does"),
    sub("tree", Status::Own, "the dependency graph as a tree"),
    sub("fetch", Status::Own, "download what the lock file names"),
    sub("clean", Status::Own, "remove what builds wrote"),
    sub("new", Status::Own, "make a package in a new directory"),
    sub("init", Status::Own, "make a package in this directory"),
    sub("pkgid", Status::Own, "the full id of a package"),
    sub("vendor", Status::Own, "copy every dependency into the workspace"),
    aka("remove", &["rm"], Status::Own, "remove dependencies from the manifest"),
    sub("add", CHOOSES_VERSIONS, "add dependencies to the manifest"),
    sub("update", CHOOSES_VERSIONS, "update the lock file"),
    sub("generate-lockfile", CHOOSES_VERSIONS, "write the lock file"),
    sub("bench", A_BUILD, "build and run the benchmarks"),
    sub("rustc", A_BUILD, "build a package with extra compiler flags"),
    sub("rustdoc", A_BUILD, "document a package with extra rustdoc flags"),
    sub("fix", A_BUILD, "apply the compiler's suggestions"),
    sub("install", A_BUILD, "build and install a program"),
    sub("uninstall", Status::Own, "remove an installed program"),
    sub("package", A_BUILD, "make the `.crate` of a package"),
    sub("publish", PUBLISH, "upload a package to a registry"),
    sub("search", Status::Own, "search a registry"),
    sub("info", INFO, "what a registry says of a package"),
    sub("owner", Status::Own, "the owners of a package on a registry"),
    sub("yank", Status::Own, "yank a version from a registry"),
    sub("login", Status::Own, "keep a registry's token"),
    sub("logout", Status::Own, "forget a registry's token"),
    sub("report", Status::Own, "reports cargo keeps of builds"),
    sub("config", Status::Own, "the configuration cargo reads"),
    sub("read-manifest", Status::Own, "the manifest as JSON (deprecated)"),
];

/// The subcommand `name` is, by its name or an alias.
pub fn named(name: &str) -> Option<&'static Subcommand> {
    SUBCOMMANDS.iter().find(|it| it.name == name || it.aliases.contains(&name))
}

/// `blaze <args>` for what is not a build: `args` starts with the subcommand. Returns
/// the status to exit with.
pub fn command(args: Vec<String>) -> anyhow::Result<i32> {
    let Some((name, rest)) = args.split_first() else { return Ok(0) };
    super::say::color(color_of(rest).as_deref());
    if name == "--list" {
        list();
        return Ok(0);
    }
    let Some(subcommand) = named(name) else {
        super::say::error(&format!("no such command: `{name}`"));
        super::say::note(
            "blaze runs no cargo and no cargo plugin; `blaze --list` lists its commands",
        );
        return Ok(super::FAILED);
    };
    match subcommand.name {
        "fmt" => Ok(fmt::command(rest)),
        "clippy" => clippy::command(rest),
        "metadata" => manifest::metadata(Args::new(rest)),
        "locate-project" => manifest::locate_project(Args::new(rest)),
        "verify-project" => manifest::verify_project(Args::new(rest)),
        "doc" => (crate::engine::get().doc)(rest),
        "tree" => tree::command(Args::new(rest)),
        "pkgid" => manifest::pkgid(Args::new(rest)),
        "read-manifest" => manifest::read_manifest(Args::new(rest)),
        "uninstall" => uninstall::command(Args::new(rest)),
        "report" => report::command(Args::new(rest)),
        "config" => config::command(rest),
        "remove" => remove::command(rest),
        "search" => registry::search(rest),
        "owner" => registry::owner(rest),
        "yank" => registry::yank(rest),
        "login" => registry::login(rest),
        "logout" => registry::logout(rest),
        "vendor" => vendor::command(Args::new(rest)),
        "fetch" => manifest::fetch(Args::new(rest)),
        "clean" => clean::command(Args::new(rest)),
        "new" | "init" => new::command(subcommand.name, Args::new(rest)),
        "version" => {
            println!("{}", crate::version());
            Ok(0)
        }
        "help" => {
            println!("{}", super::HELP);
            Ok(0)
        }
        _ => Ok(not_yet(subcommand)),
    }
}

/// What `--color` the words of a subcommand give, the last one.
fn color_of(words: &[String]) -> Option<String> {
    let mut found = None;
    let mut words = words.iter().take_while(|it| *it != "--");
    while let Some(word) = words.next() {
        match word.strip_prefix("--color") {
            Some("") => found = words.next().cloned(),
            Some(rest) => found = rest.strip_prefix('=').map(str::to_owned).or(found),
            None => {}
        }
    }
    found
}

/// Says that blaze has no `subcommand` yet and that nothing was done, as a failure.
fn not_yet(subcommand: &Subcommand) -> i32 {
    let why = match subcommand.status {
        Status::Not(why) => why,
        Status::Build | Status::Own => "not written yet",
    };
    super::say::error(&format!(
        "blaze has no `cargo {}` yet ({why}); it hands nothing to cargo, so nothing was done",
        subcommand.name
    ));
    super::say::note(&format!("what blaze has of each cargo command: {TABLE}"));
    super::FAILED
}

/// `blaze --list`: the commands blaze has, and those it has not yet.
fn list() {
    let width = SUBCOMMANDS.iter().map(|it| it.name.len()).max().unwrap_or(0);
    let (have, not): (Vec<_>, Vec<_>) =
        SUBCOMMANDS.iter().partition(|it| !matches!(it.status, Status::Not(_)));
    println!("Installed Commands:");
    for it in have {
        println!("    {:width$}  {}", it.name, it.about);
    }
    println!("Not yet blaze's (each fails, saying so):");
    for it in not {
        println!("    {:width$}  {}", it.name, it.about);
    }
}

/// `args` with cargo's flags typed before the subcommand moved after it, where every
/// subcommand reads them: `blaze -v fmt` is `blaze fmt -v`.
pub fn subcommand_first(args: Vec<String>) -> Vec<String> {
    const VALUED: &[&str] = &["--color", "--config", "-Z", "-C", "--explain"];
    let mut ahead = Vec::new();
    let mut words = args.into_iter();
    while let Some(word) = words.next() {
        let own = matches!(word.as_str(), "-h" | "--help" | "-V" | "--version" | "--list");
        if !word.starts_with('-') || own {
            return std::iter::once(word).chain(ahead).chain(words).collect();
        }
        let valued = VALUED.contains(&word.as_str());
        ahead.push(word);
        if valued {
            ahead.extend(words.next());
        }
    }
    ahead
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn every_name_is_one_subcommand() {
        let mut seen = std::collections::BTreeSet::new();
        for it in SUBCOMMANDS {
            for name in std::iter::once(&it.name).chain(it.aliases) {
                assert!(seen.insert(*name), "{name} twice");
            }
        }
        assert_eq!(named("rm").map(|it| it.name), Some("remove"));
        assert_eq!(named("d").map(|it| it.name), Some("doc"));
        assert!(named("nextest").is_none());
    }

    #[test]
    fn the_builds_are_the_ones_cli_reads() {
        for it in SUBCOMMANDS.iter().filter(|it| it.status == Status::Build) {
            for name in std::iter::once(&it.name).chain(it.aliases) {
                let asked = super::super::cli::Asked::parse(vec![(*name).to_owned()]);
                assert!(matches!(asked, super::super::cli::Asked::Build(_)), "{name}");
            }
        }
    }

    #[test]
    fn flags_ahead_go_after_the_subcommand() {
        assert_eq!(subcommand_first(words("-v fmt --check")), words("fmt -v --check"));
        assert_eq!(
            subcommand_first(words("--config a=1 -Z x build -p y")),
            words("build --config a=1 -Z x -p y")
        );
        assert_eq!(subcommand_first(words("--version")), words("--version"));
        assert_eq!(subcommand_first(words("-q")), words("-q"));
        assert_eq!(subcommand_first(Vec::new()), Vec::<String>::new());
    }

    #[test]
    fn the_last_color_given_counts() {
        assert_eq!(color_of(&words("-q --color always x")).as_deref(), Some("always"));
        assert_eq!(color_of(&words("--color=never --color always")).as_deref(), Some("always"));
        assert_eq!(color_of(&words("-- --color always")), None);
        assert_eq!(color_of(&words("--colorful")), None);
    }

    #[test]
    fn what_is_not_built_fails_and_says_so() {
        crate::engine::for_tests();
        assert_eq!(command(words("frobnicate")).unwrap(), super::super::FAILED);
        assert_eq!(command(words("publish --dry-run")).unwrap(), super::super::FAILED);
        assert_eq!(command(words("version")).unwrap(), 0);
    }
}
