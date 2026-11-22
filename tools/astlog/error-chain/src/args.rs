//! A command line read by hand, for a command's own parser to take apart: `--name value` and
//! `--name=value` flags, switches, positionals and a first word naming a subcommand. An argument
//! no reader took is an [`ArgError`] naming it; everything after `--` is positional.
//!
//! ```text
//! let mut args = Args::from_env()?;
//! let command = args.first();               // the subcommand, or a top-level flag
//! let json = args.switch(&["--json"])?;     // --json
//! let relation = args.value("--relation")?; // --relation r, --relation=r
//! let rules = args.positional();            // the first positional
//! let paths = args.finish()?;               // the rest, or the first unread flag
//! ```

use std::ffi::OsString;
use std::fmt;

/// The arguments after the program name, each taken once by the command's parser.
#[derive(Debug)]
pub struct Args {
    /// The arguments in order; `None` once taken (and for the `--` itself).
    items: Vec<Option<String>>,
    /// Where `--` was (else the end): from there on nothing is a flag.
    flags_end: usize,
}

impl Args {
    /// The process's arguments after its name.
    ///
    /// # Errors
    ///
    /// [`ArgError::NotUnicode`] for an argument that is not UTF-8.
    pub fn from_env() -> Result<Self, ArgError> {
        Self::new(std::env::args_os().skip(1))
    }

    /// # Errors
    ///
    /// [`ArgError::NotUnicode`] for an argument that is not UTF-8.
    pub fn new(args: impl IntoIterator<Item = OsString>) -> Result<Self, ArgError> {
        let mut items = Vec::new();
        for (at, arg) in args.into_iter().enumerate() {
            let arg = arg.into_string().map_err(|arg| ArgError::NotUnicode { at: at + 1, arg })?;
            items.push(Some(arg));
        }
        let flags_end = items.iter().position(|it| it.as_deref() == Some("--"));
        let flags_end = flags_end.map_or(items.len(), |at| {
            items[at] = None;
            at
        });
        Ok(Self { items, flags_end })
    }

    /// The first argument, flag or not, taken: where a command reads its subcommand.
    pub fn first(&mut self) -> Option<String> {
        self.items.iter_mut().find_map(Option::take)
    }

    /// Whether the switch was given under any of `names`, each spelled in full (`--json`, `-h`).
    ///
    /// # Errors
    ///
    /// [`ArgError::Repeated`] when given twice, [`ArgError::UnexpectedValue`] for `--json=x`.
    pub fn switch(&mut self, names: &[&str]) -> Result<bool, ArgError> {
        let mut found = false;
        for item in &mut self.items[..self.flags_end] {
            let Some(arg) = item.as_deref() else { continue };
            let flag = arg.split_once('=').map_or(arg, |(flag, _)| flag);
            if !names.contains(&flag) {
                continue;
            }
            if flag != arg {
                return Err(ArgError::UnexpectedValue { flag: flag.to_owned() });
            }
            if found {
                return Err(ArgError::Repeated { flag: flag.to_owned() });
            }
            found = true;
            *item = None;
        }
        Ok(found)
    }

    /// The value of `name` (`--relation`) given as `--relation value` or `--relation=value`.
    ///
    /// # Errors
    ///
    /// [`ArgError::MissingValue`] for a last `--relation` or one followed by a flag,
    /// [`ArgError::Repeated`] when given twice.
    pub fn value(&mut self, name: &str) -> Result<Option<String>, ArgError> {
        let mut found = None;
        for at in 0..self.flags_end {
            let Some(arg) = self.items[at].as_deref() else { continue };
            let value = if arg == name {
                let next = self.items[at + 1..self.flags_end].first_mut().and_then(Option::take);
                match next {
                    Some(value) if !is_flag(&value) => value,
                    _ => return Err(ArgError::MissingValue { flag: name.to_owned() }),
                }
            } else if let Some(value) = arg.strip_prefix(name).and_then(|it| it.strip_prefix('=')) {
                value.to_owned()
            } else {
                continue;
            };
            if found.is_some() {
                return Err(ArgError::Repeated { flag: name.to_owned() });
            }
            found = Some(value);
            self.items[at] = None;
        }
        Ok(found)
    }

    /// The first positional argument left (one not starting with `-`, or any after `--`), taken.
    pub fn positional(&mut self) -> Option<String> {
        let flags_end = self.flags_end;
        let (_, item) = self.items.iter_mut().enumerate().find(|(at, item)| {
            item.as_deref().is_some_and(|arg| *at > flags_end || !is_flag(arg))
        })?;
        item.take()
    }

    /// The positional arguments left, in order.
    ///
    /// # Errors
    ///
    /// [`ArgError::UnknownFlag`] naming the first flag no reader took.
    pub fn finish(self) -> Result<Vec<String>, ArgError> {
        let mut rest = Vec::new();
        for (at, arg) in self.items.into_iter().enumerate() {
            let Some(arg) = arg else { continue };
            if at < self.flags_end && is_flag(&arg) {
                return Err(ArgError::UnknownFlag { flag: arg });
            }
            rest.push(arg);
        }
        Ok(rest)
    }
}

fn is_flag(arg: &str) -> bool {
    arg.starts_with('-') && arg != "-"
}

/// A command line its command does not accept.
#[derive(Debug)]
pub enum ArgError {
    /// Argument `at` (1-based, after the program name) is not UTF-8.
    NotUnicode { at: usize, arg: OsString },
    UnknownFlag { flag: String },
    MissingValue { flag: String },
    UnexpectedValue { flag: String },
    Repeated { flag: String },
    /// No subcommand was given; `commands` lists the ones there are.
    MissingCommand { commands: &'static str },
    UnknownCommand { name: String, commands: &'static str },
    /// A required positional (`what`, as the usage line names it) is missing.
    Missing { what: &'static str },
    /// A positional argument the command takes none of.
    Unexpected { arg: String },
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUnicode { at, arg } => write!(f, "argument {at} is not UTF-8: {arg:?}"),
            Self::UnknownFlag { flag } => write!(f, "unknown flag `{flag}`"),
            Self::MissingValue { flag } => write!(f, "`{flag}` needs a value"),
            Self::UnexpectedValue { flag } => write!(f, "`{flag}` takes no value"),
            Self::Repeated { flag } => write!(f, "`{flag}` is given more than once"),
            Self::MissingCommand { commands } => write!(f, "no command given; one of {commands}"),
            Self::UnknownCommand { name, commands } => {
                write!(f, "unknown command `{name}`; one of {commands}")
            }
            Self::Missing { what } => write!(f, "missing {what}"),
            Self::Unexpected { arg } => write!(f, "unexpected argument `{arg}`"),
        }
    }
}

impl std::error::Error for ArgError {}

#[cfg(test)]
mod tests {
    use super::{ArgError, Args};

    fn args(line: &[&str]) -> Args {
        Args::new(line.iter().map(Into::into)).expect("UTF-8 arguments")
    }

    #[test]
    fn reads_flags_values_and_positionals_in_any_order() -> Result<(), ArgError> {
        let mut line = args(&["query", "r.astlog", "--json", "src", "--relation=x", "--", "--y"]);
        assert_eq!(line.first().as_deref(), Some("query"));
        assert!(line.switch(&["--json"])?);
        assert!(!line.switch(&["--write"])?);
        assert_eq!(line.value("--relation")?.as_deref(), Some("x"));
        assert_eq!(line.positional().as_deref(), Some("r.astlog"));
        assert_eq!(line.finish()?, ["src", "--y"]);
        let mut spaced = args(&["--relation", "x", "p"]);
        assert_eq!(spaced.value("--relation")?.as_deref(), Some("x"));
        assert_eq!(spaced.finish()?, ["p"]);
        Ok(())
    }

    #[test]
    fn refusals_name_the_flag() {
        let unknown = args(&["p", "--jsn"]).finish();
        assert!(matches!(unknown, Err(ArgError::UnknownFlag { flag }) if flag == "--jsn"));
        let missing = args(&["--relation"]).value("--relation");
        assert!(matches!(missing, Err(ArgError::MissingValue { .. })));
        let valued = args(&["--json=1"]).switch(&["--json"]);
        assert!(matches!(valued, Err(ArgError::UnexpectedValue { .. })));
        let twice = args(&["--json", "--json"]).switch(&["--json"]);
        assert!(matches!(twice, Err(ArgError::Repeated { .. })));
    }
}
