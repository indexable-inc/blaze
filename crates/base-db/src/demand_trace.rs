//! EXPERIMENT (lane relax-demand, not for landing): a line per execution of the front end's
//! demand-driven queries, when `/root/demand` exists, into `/root/demand/<pid>.tsv`.

use std::{
    fmt::Debug,
    io::Write,
    sync::{Mutex, OnceLock},
};

use crate::{Crate, CrateOrigin, SourceDatabase};

static OUT: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

fn out() -> Option<&'static Mutex<std::fs::File>> {
    OUT.get_or_init(|| {
        let dir = std::path::Path::new("/root/demand");
        if !dir.is_dir() {
            return None;
        }
        let path = dir.join(format!("{}.tsv", std::process::id()));
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
        Some(Mutex::new(file))
    })
    .as_ref()
}

pub fn origin(db: &dyn SourceDatabase, krate: Crate) -> (&'static str, String) {
    let data = krate.data(db);
    let kind = match &data.origin {
        CrateOrigin::Local { .. } => "local",
        CrateOrigin::Library { .. } => "dep",
        CrateOrigin::Lang(_) => "lang",
        CrateOrigin::Rustc { .. } => "rustc",
    };
    let name = krate
        .extra_data(db)
        .display_name
        .as_ref()
        .map_or_else(|| "?".to_owned(), |it| it.to_string());
    (kind, name)
}

pub fn record(
    kind: &str,
    db: &dyn SourceDatabase,
    krate: Option<Crate>,
    key: impl Debug,
    size: usize,
) {
    let Some(out) = out() else { return };
    let (origin, name) = krate.map_or(("?", "?".to_owned()), |it| origin(db, it));
    let line = format!("{kind}\t{origin}\t{name}\t{key:?}\t{size}\n");
    _ = out.lock().unwrap_or_else(|it| it.into_inner()).write_all(line.as_bytes());
}

