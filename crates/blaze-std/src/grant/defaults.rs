//! What an ordinary start grants (one started without a run spec on file descriptor 3): first
//! each `--grant <name>=<value>` its command line begins with, then the attribute's default for
//! each parameter no `--grant` named, then nothing. A parameter left without a grant is refused
//! before `main`, with the `--grant` line that fixes it (grant.rs's `Problem::line`).
//!
//! **One spelling.** The attribute spells a default as the test attribute spells a grant:
//!
//! ```text
//! #[bstd::main(data = dir("."), env(HOME, PATH), api = connect("example.com:443"))]
//! ```
//!
//! | Spelling                     | The parameter's kind      | The `--grant` value it is   |
//! |------------------------------|---------------------------|-----------------------------|
//! | `p = dir("path")`            | `Dir<ReadOnly>`/`<ReadWrite>` | `path`                  |
//! | `p = connect("host:port")`   | `Net`, `Connect`          | `host:port`                 |
//! | `p = listen("host:port")`    | `Net`, `Listen`           | `listen:host:port`          |
//! | `p = env(A, B)`, `env(A, B)` | `Env` (the bare form: `env`) | `A,B`                    |
//! | `p = whole`                  | a whole capability        | `whole`                     |
//! | `p = device("name")`         | a capability of one device | `device:name`              |
//!
//! [`crate::__grant_spellings!`] is the one parser of these spellings: `#[bstd::main]` calls it,
//! and so does `#[bstd::test]` for the spellings it shares (`connect`, `listen`). A spelling
//! becomes the very `--grant` value text, which spec.rs `grant_arg` reads, so a default and a
//! command-line grant are read by one reader (rule 30), and a default can always be written out
//! as the `--grant` line that would replace it.
//!
//! **Checked twice over, each once.** As the crate compiles ([`Manifest::with_defaults`], a
//! const fn the manifest's static runs): each default names a parameter, once, with a spelling
//! that fits its kind. At the start ([`ordinary_grants`]): its value reads as that kind's scope
//! (a folder path, an endpoint, variable names); a value that does not is a refusal naming the
//! attribute, before `main`. A `OneOf` or `Many` takes no default yet: its alternative or
//! members want their own spelling (`inputs.a = dir(..)`).

use blaze_own::String;
use blaze_text::Writes as _;

use super::{Form, Kind, LaunchAlloc, Manifest, Param};
use crate::spec::{GrantArgError, RunSpec, grant_arg};

/// Which kinds a spelling fits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    /// `dir(..)`: a folder.
    Dir,
    /// `connect(..)`, `listen(..)`: an endpoint.
    Net,
    /// `env(..)`: variable names.
    Env,
    /// `whole`, `device(..)`: a whole capability, or one device of it.
    Whole,
}

impl Shape {
    /// Whether a parameter of `kind` takes this spelling.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn fits(self, kind: Kind) -> bool {
        matches!(
            (self, kind),
            (Shape::Dir, Kind::Dir { .. }) | (Shape::Net, Kind::Net) | (Shape::Env, Kind::Env) | (Shape::Whole, Kind::Whole(_))
        )
    }
}

/// One grant an attribute spells: its parameter, its spelling's shape, and the `--grant` value
/// it is (`.` for `dir(".")`, `HOME,PATH` for `env(HOME, PATH)`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Spelling {
    pub name: &'static str,
    pub shape: Shape,
    pub value: &'static str,
}

impl Spelling {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new(name: &'static str, shape: Shape, value: &'static str) -> Spelling {
        Spelling { name, shape, value }
    }
}

impl Manifest {
    /// This manifest with the attribute's `defaults`, checked as the crate compiles: each names
    /// a parameter of `main`, once, with a spelling its kind takes, and none is for a `OneOf`
    /// or a `Many`. A failed check is a compile error at the manifest's static.
    pub const fn with_defaults(mut self, defaults: &'static [Spelling]) -> Manifest {
        let mut at = 0;
        while at < defaults.len() {
            let default = defaults[at];
            let Some(param) = param_named(self.params, default.name) else {
                panic!("bstd::main: a default names no parameter of `main`");
            };
            if !default.shape.fits(param.kind) {
                panic!(
                    "bstd::main: a default's spelling does not fit its parameter: `dir(..)` is a `Dir`'s, \
                     `connect(..)`/`listen(..)` a `Net`'s, `env(..)` an `Env`'s, `whole`/`device(..)` a whole capability's"
                );
            }
            if !matches!(param.form, Form::One | Form::Optional) {
                panic!("bstd::main: a `OneOf` or `Many` parameter takes no default yet: grant it with `--grant`");
            }
            let mut before = 0;
            while before < at {
                if super::same(defaults[before].name, default.name) {
                    panic!("bstd::main: a parameter is given two defaults");
                }
                before += 1;
            }
            at += 1;
        }
        self.defaults = defaults;
        self
    }

    /// The attribute's default for the parameter `name`.
    pub fn default_of(&self, name: &str) -> Option<&Spelling> {
        self.defaults.iter().find(|default| default.name == name)
    }
}

/// The parameter of `params` named `name`, in a const fn.
const fn param_named(params: &'static [Param], name: &str) -> Option<Param> {
    let mut at = 0;
    while at < params.len() {
        if super::same(params[at].name, name) {
            return Some(params[at]);
        }
        at += 1;
    }
    None
}

/// The grants of an ordinary start, put into `spec`: each leading `--grant <name>=<value>` (or
/// `--grant=<name>=<value>`) of `args`, after the program's name, until the first other
/// argument (a `--` ends them and is taken too); then each default of a parameter no `--grant`
/// named. How many arguments after the program's name it took: `main`'s `Args` does not see
/// them.
pub fn ordinary_grants<'a>(
    manifest: &Manifest,
    args: &[&'a str],
    spec: &mut RunSpec<LaunchAlloc>,
    alloc: LaunchAlloc,
) -> Result<usize, OrdinaryError<'a>> {
    let mut at = 1;
    while let Some(arg) = args.get(at).copied() {
        let (grant, took) = match arg {
            "--" => {
                at += 1;
                break;
            }
            "--grant" => (args.get(at + 1).copied().ok_or(OrdinaryError::NoValue)?, 2),
            _ => match arg.strip_prefix("--grant=") {
                Some(grant) => (grant, 1),
                None => break,
            },
        };
        let granted = grant_arg(manifest, grant, alloc).map_err(|why| OrdinaryError::Argument { grant, why })?;
        if spec.grant(granted.name.as_str()).is_some() {
            return Err(OrdinaryError::Twice { grant });
        }
        // alloc: one grant of the ordinary start's spec, in the launcher's allocator.
        spec.put_grant(granted);
        at += took;
    }
    for default in manifest.defaults {
        if spec.grant(default.name).is_some() {
            continue;
        }
        // alloc: the default's `--grant` line, once per default, in the launcher's allocator.
        let mut line = String::new_in(alloc);
        line.str(default.name).str("=").str(default.value);
        let granted = grant_arg(manifest, line.as_str(), alloc).map_err(|why| OrdinaryError::Default { default: *default, why })?;
        spec.put_grant(granted);
    }
    Ok(at.saturating_sub(1))
}

/// Why an ordinary start's grants were refused: the launcher prints it and exits refused,
/// before `main`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OrdinaryError<'a> {
    /// `--grant` ended the command line.
    NoValue,
    /// A `--grant` that does not read.
    Argument { grant: &'a str, why: GrantArgError },
    /// Two `--grant`s of one name.
    Twice { grant: &'a str },
    /// The attribute's default whose value does not read as its parameter's scope.
    Default { default: Spelling, why: GrantArgError },
}

impl core::fmt::Display for OrdinaryError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            OrdinaryError::NoValue => f.write_str("bstd: `--grant` wants `<parameter>=<scope>` after it"),
            OrdinaryError::Argument { grant, why } => write!(f, "bstd: `--grant {grant}`: {why}"),
            OrdinaryError::Twice { grant } => write!(f, "bstd: `--grant {grant}`: that parameter is granted twice"),
            OrdinaryError::Default { default, why } => write!(
                f,
                "bstd: the default of `{}` in `#[bstd::main(..)]` (`--grant {}={}`): {why}",
                default.name, default.name, default.value
            ),
        }
    }
}

impl core::error::Error for OrdinaryError<'_> {}

/// The one parser of a grant's spelling (the module's doc): an array of [`Spelling`]s from the
/// attribute's arguments, or a compile error naming what it does not take.
#[doc(hidden)]
#[macro_export]
macro_rules! __grant_spellings {
    (@ [$($done:expr),*] ;) => { [$($done),*] };
    (@ [$($done:expr),*] ; env($first:ident $(, $var:ident)* $(,)?) $(, $($rest:tt)*)?) => {
        $crate::__grant_spellings!(@ [$($done,)* $crate::grant::Spelling::new(
            "env", $crate::grant::Shape::Env, ::core::concat!(::core::stringify!($first) $(, ",", ::core::stringify!($var))*)
        )] ; $($($rest)*)?)
    };
    (@ [$($done:expr),*] ; $g:ident = env($first:ident $(, $var:ident)* $(,)?) $(, $($rest:tt)*)?) => {
        $crate::__grant_spellings!(@ [$($done,)* $crate::grant::Spelling::new(
            ::core::stringify!($g), $crate::grant::Shape::Env, ::core::concat!(::core::stringify!($first) $(, ",", ::core::stringify!($var))*)
        )] ; $($($rest)*)?)
    };
    (@ [$($done:expr),*] ; $g:ident = dir($path:literal) $(, $($rest:tt)*)?) => {
        $crate::__grant_spellings!(@ [$($done,)* $crate::grant::Spelling::new(::core::stringify!($g), $crate::grant::Shape::Dir, $path)] ; $($($rest)*)?)
    };
    (@ [$($done:expr),*] ; $g:ident = connect($at:literal) $(, $($rest:tt)*)?) => {
        $crate::__grant_spellings!(@ [$($done,)* $crate::grant::Spelling::new(::core::stringify!($g), $crate::grant::Shape::Net, $at)] ; $($($rest)*)?)
    };
    (@ [$($done:expr),*] ; $g:ident = listen($at:literal) $(, $($rest:tt)*)?) => {
        $crate::__grant_spellings!(@ [$($done,)* $crate::grant::Spelling::new(
            ::core::stringify!($g), $crate::grant::Shape::Net, ::core::concat!("listen:", $at)
        )] ; $($($rest)*)?)
    };
    (@ [$($done:expr),*] ; $g:ident = whole $(, $($rest:tt)*)?) => {
        $crate::__grant_spellings!(@ [$($done,)* $crate::grant::Spelling::new(::core::stringify!($g), $crate::grant::Shape::Whole, "whole")] ; $($($rest)*)?)
    };
    (@ [$($done:expr),*] ; $g:ident = device($device:literal) $(, $($rest:tt)*)?) => {
        $crate::__grant_spellings!(@ [$($done,)* $crate::grant::Spelling::new(
            ::core::stringify!($g), $crate::grant::Shape::Whole, ::core::concat!("device:", $device)
        )] ; $($($rest)*)?)
    };
    (@ [$($done:expr),*] ; $($other:tt)*) => {
        ::core::compile_error!(::core::concat!(
            "bstd::main: a default it does not take: `", ::core::stringify!($($other)*),
            "`; it takes `<dir> = dir(\"path\")`, `<net> = connect(\"host:port\")` or `listen(..)`, ",
            "`env(NAME, ..)` or `<env> = env(NAME, ..)`, `<capability> = whole` or `device(\"name\")`"
        ))
    };
    ($($arg:tt)*) => { $crate::__grant_spellings!(@ [] ; $($arg)*) };
}

#[cfg(test)]
mod tests;
