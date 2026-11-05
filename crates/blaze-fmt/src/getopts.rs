//! The part of the `getopts` crate (0.2) rustfmt's command line uses: options declared by short
//! and long name, parsed GNU style with free arguments anywhere, and the same usage text, so
//! `cli.rs` reads as upstream does.

use std::fmt;

#[derive(Clone, Copy, PartialEq)]
enum HasArg {
    No,
    Yes,
    Maybe,
}

#[derive(Clone, Copy, PartialEq)]
enum Occur {
    Optional,
    Multi,
}

struct Opt {
    short: String,
    long: String,
    hint: String,
    desc: String,
    hasarg: HasArg,
    occur: Occur,
}

#[derive(Default)]
pub(crate) struct Options {
    opts: Vec<Opt>,
}

#[derive(Debug)]
pub(crate) enum Fail {
    ArgumentMissing(String),
    UnrecognizedOption(String),
    OptionDuplicated(String),
    UnexpectedArgument(String),
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fail::ArgumentMissing(nm) => write!(f, "Argument to option '{nm}' missing"),
            Fail::UnrecognizedOption(nm) => write!(f, "Unrecognized option: '{nm}'"),
            Fail::OptionDuplicated(nm) => write!(f, "Option '{nm}' given more than once"),
            Fail::UnexpectedArgument(nm) => write!(f, "Option '{nm}' does not take an argument"),
        }
    }
}

impl std::error::Error for Fail {}

enum Val {
    Given,
    Arg(String),
}

pub(crate) struct Matches {
    names: Vec<(String, String)>,
    vals: Vec<Vec<Val>>,
    pub(crate) free: Vec<String>,
}

impl Options {
    pub(crate) fn new() -> Options {
        Options::default()
    }

    fn push(
        &mut self,
        short: &str,
        long: &str,
        desc: &str,
        hint: &str,
        hasarg: HasArg,
        occur: Occur,
    ) {
        self.opts.push(Opt {
            short: short.to_owned(),
            long: long.to_owned(),
            hint: hint.to_owned(),
            desc: desc.to_owned(),
            hasarg,
            occur,
        });
    }

    pub(crate) fn optflag(&mut self, short: &str, long: &str, desc: &str) -> &mut Options {
        self.push(short, long, desc, "", HasArg::No, Occur::Optional);
        self
    }

    pub(crate) fn optopt(
        &mut self,
        short: &str,
        long: &str,
        desc: &str,
        hint: &str,
    ) -> &mut Options {
        self.push(short, long, desc, hint, HasArg::Yes, Occur::Optional);
        self
    }

    pub(crate) fn optmulti(
        &mut self,
        short: &str,
        long: &str,
        desc: &str,
        hint: &str,
    ) -> &mut Options {
        self.push(short, long, desc, hint, HasArg::Yes, Occur::Multi);
        self
    }

    pub(crate) fn optflagopt(
        &mut self,
        short: &str,
        long: &str,
        desc: &str,
        hint: &str,
    ) -> &mut Options {
        self.push(short, long, desc, hint, HasArg::Maybe, Occur::Optional);
        self
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.opts
            .iter()
            .position(|o| o.long == name || (name.chars().count() == 1 && o.short == name))
    }

    pub(crate) fn parse<I, S>(&self, args: I) -> Result<Matches, Fail>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut vals: Vec<Vec<Val>> = self.opts.iter().map(|_| Vec::new()).collect();
        let mut free = Vec::new();
        let mut args = args.into_iter().map(|a| a.as_ref().to_owned()).peekable();
        let is_opt = |a: &str| a.len() > 1 && a.starts_with('-');
        while let Some(arg) = args.next() {
            if arg == "--" {
                free.extend(args.by_ref());
                break;
            }
            if !is_opt(&arg) {
                free.push(arg);
                continue;
            }
            // (name, argument attached with `=` or after a short name, was it a long name)
            let mut names: Vec<(String, Option<String>, bool)> = Vec::new();
            if let Some(long) = arg.strip_prefix("--") {
                match long.split_once('=') {
                    Some((n, v)) => names.push((n.to_owned(), Some(v.to_owned()), true)),
                    None => names.push((long.to_owned(), None, true)),
                }
            } else {
                let shorts: Vec<char> = arg[1..].chars().collect();
                for (i, c) in shorts.iter().enumerate() {
                    let name = c.to_string();
                    let takes_arg = self
                        .find(&name)
                        .is_some_and(|id| self.opts[id].hasarg != HasArg::No);
                    if takes_arg && i + 1 < shorts.len() {
                        names.push((name, Some(shorts[i + 1..].iter().collect()), false));
                        break;
                    }
                    names.push((name, None, false));
                }
            }
            for (name, attached, was_long) in names {
                let id = self
                    .find(&name)
                    .ok_or_else(|| Fail::UnrecognizedOption(name.clone()))?;
                let opt = &self.opts[id];
                match opt.hasarg {
                    HasArg::No => {
                        if attached.is_some() {
                            return Err(Fail::UnexpectedArgument(name));
                        }
                        vals[id].push(Val::Given);
                    }
                    HasArg::Yes => match attached {
                        Some(v) => vals[id].push(Val::Arg(v)),
                        None => match args.next() {
                            Some(v) => vals[id].push(Val::Arg(v)),
                            None => return Err(Fail::ArgumentMissing(name)),
                        },
                    },
                    // `--name value` is not taken, as GNU getopt does not.
                    HasArg::Maybe => match attached {
                        Some(v) => vals[id].push(Val::Arg(v)),
                        None if was_long || args.peek().is_none_or(|n| is_opt(n)) => {
                            vals[id].push(Val::Given);
                        }
                        None => vals[id].push(Val::Arg(args.next().unwrap_or_default())),
                    },
                }
                if opt.occur == Occur::Optional && vals[id].len() > 1 {
                    return Err(Fail::OptionDuplicated(name));
                }
            }
        }
        Ok(Matches {
            names: self
                .opts
                .iter()
                .map(|o| (o.short.clone(), o.long.clone()))
                .collect(),
            vals,
            free,
        })
    }

    pub(crate) fn usage(&self, brief: &str) -> String {
        const DESC_COLUMN: usize = 24;
        const DESC_WIDTH: usize = 54;
        let any_short = self.opts.iter().any(|o| !o.short.is_empty());
        let desc_sep = format!("\n{}", " ".repeat(DESC_COLUMN));
        let rows: Vec<String> = self
            .opts
            .iter()
            .map(|o| {
                let mut row = "    ".to_owned();
                if o.short.is_empty() {
                    if any_short {
                        row.push_str("    ");
                    }
                } else {
                    row.push('-');
                    row.push_str(&o.short);
                    row.push_str(if o.long.is_empty() { " " } else { ", " });
                }
                if !o.long.is_empty() {
                    row.push_str("--");
                    row.push_str(&o.long);
                    row.push(' ');
                }
                match o.hasarg {
                    HasArg::No => {}
                    HasArg::Yes => row.push_str(&o.hint),
                    HasArg::Maybe => {
                        row.push('[');
                        row.push_str(&o.hint);
                        row.push(']');
                    }
                }
                let len = row.chars().count();
                if len < DESC_COLUMN {
                    row.push_str(&" ".repeat(DESC_COLUMN - len));
                } else {
                    row.push_str(&desc_sep);
                }
                row.push_str(&wrap(&o.desc, DESC_WIDTH).join(&desc_sep));
                row
            })
            .collect();
        format!("{brief}\n\nOptions:\n{}\n", rows.join("\n"))
    }
}

/// Greedy word wrap of `text` (whitespace normalised) into lines of at most `width` columns.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    lines
}

impl Matches {
    fn vals(&self, name: &str) -> &[Val] {
        self.names
            .iter()
            .position(|(s, l)| l == name || s == name)
            .map_or(&[], |id| &self.vals[id])
    }

    pub(crate) fn opt_present(&self, name: &str) -> bool {
        !self.vals(name).is_empty()
    }

    pub(crate) fn opt_str(&self, name: &str) -> Option<String> {
        self.vals(name).iter().find_map(|v| match v {
            Val::Arg(s) => Some(s.clone()),
            Val::Given => None,
        })
    }

    pub(crate) fn opt_strs(&self, name: &str) -> Vec<String> {
        self.vals(name)
            .iter()
            .filter_map(|v| match v {
                Val::Arg(s) => Some(s.clone()),
                Val::Given => None,
            })
            .collect()
    }
}
