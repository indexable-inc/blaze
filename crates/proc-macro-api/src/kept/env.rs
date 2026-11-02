//! The environment an expansion is given besides its crate's variables: what blaze sets for
//! every macro ([`SET`]) and what the user's shell passes through, as the daemon was last told
//! ([`Environment::pass`]). A kept expansion is checked against it ([`super::Expansions`]).

use std::{
    ffi::OsString,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use blaze_ids::{Hasher, Idx, Interner as _, Sharded};

/// The variables blaze sets for every expansion, whatever it expands.
pub const SET: [(&str, &str); 1] = [("RUST_ANALYZER_INTERNALS_DO_NOT_USE", "this is unstable")];

/// What the user's shell passes through to the expansions, besides [`SET`].
///
/// Every environment passed is kept once, by its content ID, in a table whose values never
/// move, and the one in use is its index behind one atomic: an expansion borrows it with one
/// acquire load, and a `pass` publishes another with one release store. A shell has few
/// environments, so keeping each is what lets a reader never wait or copy.
#[derive(Default)]
pub struct Environment {
    all: Sharded<Vec<(String, String)>>,
    /// The index of the one in use, plus one; 0 before the first `pass`.
    now: AtomicU32,
}

impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment").field("passed", &self.passed()).finish()
    }
}

impl Environment {
    pub fn new() -> Arc<Environment> {
        // alloc: once per daemon.
        Arc::new(Environment::default())
    }

    /// The variables the user's shell passes through to the expansions asked for from now on.
    /// A variable whose name or value is not UTF-8 can't be given to a macro as `proc_macro`
    /// gives it, so it is refused, naming it: an expansion would not see it as the macro under
    /// rustc would.
    pub fn pass(&self, env: Vec<(OsString, OsString)>) -> Result<(), String> {
        let mut passed = Vec::with_capacity(env.len());
        for (name, value) in env {
            match (name.into_string(), value.into_string()) {
                (Ok(name), Ok(value)) => passed.push((name, value)),
                (Ok(name), Err(_)) => {
                    // alloc: error path, naming the variable.
                    return Err(format!(
                        "the variable {name} is not UTF-8, so no proc macro can be given it"
                    ));
                }
                (Err(name), _) => {
                    // alloc: error path, naming the variable.
                    return Err(format!(
                        "a variable named {name:?} is not UTF-8, so no proc macro can be given it"
                    ));
                }
            }
        }
        passed.sort();
        let mut id = Hasher::new("blaze v2 passed environment 1");
        id.update(&(passed.len() as u64).to_le_bytes());
        for (name, value) in &passed {
            id.part(name.as_bytes()).part(value.as_bytes());
        }
        let at = self.all.intern(id.finish(), || passed);
        self.now.store(at.raw() + 1, Ordering::Release);
        Ok(())
    }

    /// The passed-through variables now: one expansion is given and is checked against one
    /// snapshot of them, borrowed where it is kept.
    pub fn passed(&self) -> &[(String, String)] {
        match self.now.load(Ordering::Acquire) {
            0 => &[],
            at => self.all.get(Idx::from_raw(at - 1)).as_slice(),
        }
    }

    /// The environment an expansion sees that is given `passed` and `call`: [`SET`], then
    /// `passed` over it, then `call` over that. Sorted by name, each once; owned, for a caller
    /// that keeps it.
    pub fn seen<'a>(
        passed: &'a [(String, String)],
        call: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Vec<(OsString, OsString)> {
        let passed = passed.iter().map(|(name, value)| (name.as_str(), value.as_str()));
        let all = SET.iter().copied().chain(passed).chain(call);
        // alloc: the environment the caller keeps, one `Vec` and each name and value.
        let mut seen = Vec::with_capacity(all.size_hint().0);
        seen.extend(all.map(|(name, value)| (OsString::from(name), OsString::from(value))));
        // Stable, so each name's last is last of its run, and kept: the one set last wins.
        seen.sort_by(|a, b| a.0.cmp(&b.0));
        seen.dedup_by(|later, kept| {
            let same = later.0 == kept.0;
            if same {
                std::mem::swap(later, kept);
            }
            same
        });
        seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_is_what_blaze_sets_then_the_shells_then_the_crates() {
        let passed = [("PATH".to_owned(), "/a".to_owned()), ("X".to_owned(), "shell".to_owned())];
        let call = [("X".to_owned(), "crate".to_owned())];
        let seen = Environment::seen(&passed, call.iter().map(|(n, v)| (n.as_str(), v.as_str())));
        let get = |name: &str| seen.iter().find(|it| it.0 == name).map(|it| it.1.clone());
        assert_eq!(get("X"), Some("crate".into()));
        assert_eq!(get("PATH"), Some("/a".into()));
        assert_eq!(get(SET[0].0), Some(SET[0].1.into()));
        assert!(seen.windows(2).all(|it| it[0].0 < it[1].0), "sorted by name, each once: {seen:?}");
    }

    #[test]
    fn what_was_passed_stays_borrowable_after_the_next_pass_and_one_seen_again_is_kept_once() {
        let environment = Environment::new();
        assert!(environment.passed().is_empty());
        environment.pass(vec![("B".into(), "2".into()), ("A".into(), "1".into())]).unwrap();
        let first = environment.passed();
        assert_eq!(first, [("A".to_owned(), "1".to_owned()), ("B".to_owned(), "2".to_owned())]);
        environment.pass(vec![("A".into(), "other".into())]).unwrap();
        assert_eq!(environment.passed(), [("A".to_owned(), "other".to_owned())]);
        assert_eq!(first[0].1, "1");
        environment.pass(vec![("A".into(), "1".into()), ("B".into(), "2".into())]).unwrap();
        assert!(std::ptr::eq(environment.passed(), first));
        assert_eq!(environment.all.len(), 2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for n in 0..100 {
                    environment.pass(vec![("N".into(), (n % 3).to_string().into())]).unwrap();
                }
            });
            for _ in 0..100 {
                let passed = environment.passed();
                assert!(passed.len() <= 2, "{passed:?}");
            }
        });
        assert_eq!(environment.all.len(), 5);
    }

    #[cfg(unix)]
    #[test]
    fn a_variable_that_is_not_utf8_is_refused_by_name() {
        use std::os::unix::ffi::OsStringExt as _;
        let environment = Environment::new();
        let bad = OsString::from_vec(vec![0xff]);
        let said = environment.pass(vec![("NAME".into(), bad)]).unwrap_err();
        assert!(said.contains("NAME"));
    }
}
