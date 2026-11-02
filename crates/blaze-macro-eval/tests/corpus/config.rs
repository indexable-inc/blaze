//! What the harness running the corpus gives it: one JSON value in `BLAZE_MACRO_CORPUS`, the
//! value itself or a file's path, refusing a field it does not know.

use serde::Deserialize;

pub(crate) const VAR: &str = "BLAZE_MACRO_CORPUS";

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct Corpus {
    /// `tests/corpus/ws` or a copy of it, absolute, built there by `blaze build -p
    /// corpus-calls` (each build script's run left its output where the plan names it), its
    /// registry packages fetched into `cargo_home`.
    pub(crate) workspace: String,
    /// Another copy, built the same way, which the keeping tests change.
    pub(crate) scratch: Option<String>,
    /// The `CARGO_HOME` the packages were fetched into and the build ran with.
    pub(crate) cargo_home: String,
}

impl Corpus {
    pub(crate) fn read() -> Result<Corpus, String> {
        let value = std::env::var(VAR).map_err(|_| {
            format!("{VAR} names no corpus config: the harness sets it (crates/blaze-macro-eval/tests/corpus.rs)")
        })?;
        let text = match value.trim_start().starts_with('{') {
            true => value,
            false => std::fs::read_to_string(&value).map_err(|error| format!("{VAR}={value}: {error}"))?,
        };
        serde_json::from_str(&text).map_err(|error| format!("{VAR}: {error}"))
    }

    pub(crate) fn scratch(&self) -> Result<&str, String> {
        self.scratch.as_deref().ok_or_else(|| format!("{VAR} names no `scratch`: a built copy of the workspace to change"))
    }
}
