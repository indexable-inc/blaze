//! The lint options of rustc's command line: `rustc_session`'s `lint_opts` and `lint_cap`, as
//! `get_cmd_lint_options` reads them.

use crate::Level;

/// The lint flags a crate is checked with. base-db holds one per crate.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct LintOptions {
    /// `-A`, `-W`, `--force-warn`, `-D` and `-F`, in the order given: the lint, group or
    /// tool's lint, with `_` for `-`, and its level.
    pub opts: Vec<(String, Level)>,
    /// `--cap-lints`.
    pub cap: Option<Level>,
}

/// The flags that set a level, in the forms rustc takes them.
const LEVEL_FLAGS: [(&str, Level); 9] = [
    ("--allow", Level::Allow),
    ("--warn", Level::Warn),
    ("--force-warn", Level::ForceWarn),
    ("--deny", Level::Deny),
    ("--forbid", Level::Forbid),
    ("-A", Level::Allow),
    ("-W", Level::Warn),
    ("-D", Level::Deny),
    ("-F", Level::Forbid),
];

impl LintOptions {
    /// The lint options of `args`, every one of which is a lint flag (`-A x`, `-Ax`,
    /// `--allow x`, `--allow=x` and so on, `--cap-lints`); any other argument is an error.
    pub fn parse(args: &[String]) -> Result<LintOptions, String> {
        let mut options = LintOptions::default();
        let mut args = Args::new(args);
        while let Some(arg) = args.arg() {
            if !options.take(&mut args, arg)? {
                // alloc: an error's text
                return Err(format!("`{arg}` is not a lint flag"));
            }
        }
        Ok(options)
    }

    /// Reads `arg`, the argument `args` gave last, when it is a lint flag, with its value;
    /// whether it was one.
    pub fn take<'a>(&mut self, args: &mut Args<'a>, arg: &'a str) -> Result<bool, String> {
        if let Some(cap) = args.value(arg, "--cap-lints")? {
            if self.cap.is_some() {
                // alloc: an error's text
                return Err("Option 'cap-lints' given more than once".to_owned());
            }
            // alloc: an error's text
            self.cap = Some(Level::of_cap(cap).ok_or_else(|| format!("unknown lint level: `{cap}`"))?);
            return Ok(true);
        }
        for (flag, level) in LEVEL_FLAGS {
            let Some(name) = args.value(arg, flag)? else { continue };
            if name == "help" {
                // alloc: an error's text
                return Err(format!("`{} help` asks for rustc's list of lints, which blaze does not write", level.to_cmd_flag()));
            }
            // alloc: setup, the command line read once per crate
            self.opts.push((name.replace('-', "_"), level));
            return Ok(true);
        }
        Ok(false)
    }
}

/// rustc's arguments, read one at a time; a flag's value is given as `-Xv`, `-X v`,
/// `--flag v` or `--flag=v`.
pub struct Args<'a> {
    rest: std::slice::Iter<'a, String>,
}

impl<'a> Args<'a> {
    pub fn new(args: &'a [String]) -> Args<'a> {
        Args { rest: args.iter() }
    }

    /// The next argument.
    pub fn arg(&mut self) -> Option<&'a str> {
        self.rest.next().map(String::as_str)
    }

    /// The value of `flag` when `arg`, the argument [`Args::arg`] gave last, is it; the next
    /// argument when the value is not in `arg`.
    pub fn value(&mut self, arg: &'a str, flag: &str) -> Result<Option<&'a str>, String> {
        if arg == flag {
            return match self.arg() {
                Some(next) => Ok(Some(next)),
                // alloc: an error's text
                None => Err(format!("Argument to option '{}' missing", flag.trim_start_matches('-'))),
            };
        }
        let rest = arg.strip_prefix(flag);
        Ok(match flag.starts_with("--") {
            true => rest.and_then(|it| it.strip_prefix('=')),
            false => rest,
        })
    }
}
