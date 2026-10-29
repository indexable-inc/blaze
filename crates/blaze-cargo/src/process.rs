//! The variables of the process a build is for. `env!` and `option_env!` read them where cargo
//! and the build scripts set nothing, and a client of the daemon sends its own with a request:
//! the daemon's are those of whoever started it.

use ide_db::{
    RootDatabase,
    base_db::{Crate, Env, ProcessEnv, process_env_var, set_process_env},
};

/// Between two variables on the line of a request.
const BETWEEN: char = '\u{1f}';

/// The variables of this process. One that is not Unicode is one `env!` can't read either.
pub fn own() -> Env {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

/// An opaque macro can read any inherited variable. Keep only a digest in code identity.
pub fn fingerprint(env: &Env) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hash = Sha256::new();
    let mut vars: Vec<_> = env.iter().collect();
    vars.sort_unstable();
    for (key, value) in vars {
        for field in [key, value] {
            hash.update((field.len() as u64).to_le_bytes());
            hash.update(field.as_bytes());
        }
    }
    crate::content::hex(&hash.finalize())
}

/// `env` on one line with no tab in it.
pub fn encode(env: &Env) -> String {
    let mut line = String::new();
    for (key, value) in Vec::from(env.clone()) {
        if !line.is_empty() {
            line.push(BETWEEN);
        }
        escape(&key, &mut line);
        line.push('=');
        escape(&value, &mut line);
    }
    line
}

/// What [`encode`] made `line` from. The name of a variable has no `=` in it.
pub fn decode(line: &str) -> Env {
    line.split(BETWEEN)
        .filter_map(|it| it.split_once('='))
        .map(|(key, value)| (unescape(key), unescape(value)))
        .collect()
}

fn escape(text: &str, line: &mut String) {
    for c in text.chars() {
        match c {
            '\\' => line.push_str("\\\\"),
            '\t' => line.push_str("\\t"),
            '\n' => line.push_str("\\n"),
            BETWEEN => line.push_str("\\s"),
            c => line.push(c),
        }
    }
}

fn unescape(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (c, c == '\\').1.then(|| chars.next()).flatten() {
            Some('t') => plain.push('\t'),
            Some('n') => plain.push('\n'),
            Some('s') => plain.push(BETWEEN),
            Some(other) => plain.push(other),
            None => plain.push(c),
        }
    }
    plain
}

/// Whether builds are for a process with other variables than `env`.
pub fn differs(db: &RootDatabase, env: &Env) -> bool {
    ProcessEnv::try_get(db).is_none_or(|it| it.vars(db) != env)
}

/// Has builds from here on be for a process with the variables `env`. The result is the crates
/// that have asked for a variable which is another now.
pub fn turn(db: &mut RootDatabase, env: Env) -> Vec<Crate> {
    let mut reading: Vec<Crate> = db
        .env_read()
        .into_iter()
        .filter(|(krate, key)| {
            !krate.env(db).contains_key(key) && process_env_var(db, key) != env.get(key)
        })
        .map(|(krate, _)| krate)
        .collect();
    if let Some(path) = hir_ty_codegen::assembler_path(db)
        && path != env.get("PATH")
    {
        // An external assembler can itself invoke tools through PATH. Its environment is
        // part of the code producer, even in crates that do not read env! themselves.
        reading.extend(ide_db::base_db::all_crates(db).iter().copied());
    }
    reading.sort_unstable();
    reading.dedup();
    set_process_env(db, env);
    reading
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_read_back_as_it_was_written() {
        let env: Env = [
            ("PLAIN".to_owned(), "value".to_owned()),
            ("EMPTY".to_owned(), String::new()),
            ("ODD".to_owned(), "a\tb\nc\\d=e\u{1f}f\\t".to_owned()),
        ]
        .into_iter()
        .collect();
        let line = encode(&env);
        assert!(!line.contains(['\t', '\n']));
        assert_eq!(decode(&line), env);
    }
}
