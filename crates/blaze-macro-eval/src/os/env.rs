//! The environment a macro reads: the calling crate's, which `getenv` answers by name and
//! `environ` gives whole, each read noted. Setting a variable would set it for the whole
//! compiler, so it stops the expansion.

use blaze_usercode::trace::{ALL_ENV, Seen};

use super::{Done, Noted, Os, ok};
use crate::guest::{Guest, Stop};

/// The longest name `getenv` reads.
const NAME_MAX: usize = 32 << 10;

impl<'a> Os<'a> {
    pub(super) fn getenv(&mut self, g: &mut impl Guest, ret: &mut [u8], addr: u64) -> Done {
        let name = g.read_c_str(addr, NAME_MAX)?;
        let Ok(name) = std::str::from_utf8(name) else {
            // alloc: the message of an expansion that can't be keyed (error path)
            let why = format!("read the variable {:?}, whose name is not UTF-8", String::from_utf8_lossy(name));
            self.unkeyable.get_or_insert(why);
            return ok(ret, 0);
        };
        if let Some(&(_, value)) = self.answers.iter().find(|it| it.0 == name) {
            return ok(ret, value);
        }
        self.trace.note(Noted::Seen(Seen::Env), name);
        let Some((name, value)) = self.env.iter().find(|it| it.0 == name) else { return ok(ret, 0) };
        self.bytes.clear();
        self.bytes.extend_from_slice(value.as_bytes());
        self.bytes.push(0);
        let value = g.alloc(&self.bytes, 1)?;
        self.answers.push((name, value));
        ok(ret, value)
    }

    /// `_NSGetEnviron()` on macOS, std's `environ()` on Linux: the address of the variable that
    /// holds the array of `NAME=value` strings, the calling crate's variables sorted by name,
    /// made once.
    pub(super) fn environ(&mut self, g: &mut impl Guest, ret: &mut [u8]) -> Done {
        if !self.trace.all_env {
            self.trace.all_env = true;
            self.trace.note(Noted::Seen(Seen::Env), ALL_ENV);
        }
        if let Some(cell) = self.environ {
            return ok(ret, cell);
        }
        // alloc: once per expansion that walks the whole environment
        let mut vars: Vec<(&str, &str)> = self.env.iter().collect();
        vars.sort_unstable();
        self.bytes.clear();
        // alloc: as above
        let mut starts = Vec::with_capacity(vars.len());
        for (name, value) in &vars {
            starts.push(self.bytes.len() as u64);
            self.bytes.extend_from_slice(name.as_bytes());
            self.bytes.push(b'=');
            self.bytes.extend_from_slice(value.as_bytes());
            self.bytes.push(0);
        }
        let strings = g.alloc(&self.bytes, 1)?;
        self.bytes.clear();
        for start in starts {
            self.bytes.extend_from_slice(&(strings + start).to_le_bytes());
        }
        self.bytes.extend_from_slice(&0u64.to_le_bytes());
        let array = g.alloc(&self.bytes, 8)?;
        let cell = g.alloc(&array.to_le_bytes(), 8)?;
        self.environ = Some(cell);
        ok(ret, cell)
    }
}

/// Why `setenv`, `unsetenv` or `putenv` (`verb`, with `"NAME=value"` at `addr` for
/// `putenv`, else the name) stops the expansion.
pub(super) fn sets(g: &impl Guest, verb: &str, addr: u64, putenv: bool) -> Stop {
    let name = match g.read_c_str(addr, NAME_MAX) {
        Ok(name) => name,
        Err(stop) => return stop,
    };
    let name = match putenv {
        true => name.split(|&it| it == b'=').next().unwrap_or(name),
        false => name,
    };
    // alloc: error path
    let name = String::from_utf8_lossy(name);
    // alloc: error path
    Stop(format!("{verb} the environment variable `{name}`, which would change it for the whole compiler"))
}
