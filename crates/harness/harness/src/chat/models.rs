//! The models each agent kind last listed (`models` events), kept by the daemon across its restarts in
//! `$HARNESS_HOME/models.cbor`, so a chat whose own log has none (one from before chats listed them, or one whose agent
//! is not running) can still choose: the picker shows this list ([`harness_proto::msg::Reply::Sessions`]'s `models`)
//! and [`super::Chat::choose`] checks against it. Every claude (or codex) that starts lists its models again, and the
//! newest list replaces the one kept.
//!
//! The file is one deterministic-CBOR map, kind to models. It is written beside (`models.cbor.tmp`), synced, renamed
//! over, and the directory synced, so a crash leaves the old list or the new one. A file that does not decode is
//! renamed aside (`models.cbor.corrupt-<ms>`) and named on stderr, never read as empty in place.
use harness_proto::event::ModelChoice;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// By agent kind, the models it last listed.
pub type ByKind = BTreeMap<String, Vec<ModelChoice>>;

/// The file's name under `$HARNESS_HOME`.
pub const FILE: &str = "models.cbor";

/// The kept lists and the file they persist to.
pub struct Cache {
    path: PathBuf,
    /// Shared with readers as a snapshot, replaced whole on a change.
    by_kind: Arc<ByKind>,
}

impl Cache {
    /// The lists kept at `path`: none when there is no file yet; a file that does not decode is renamed aside (named in
    /// the error's place on stderr) and the cache starts empty.
    pub fn restore(path: PathBuf) -> Self {
        let by_kind = match std::fs::read(&path) {
            Ok(bytes) => match harness_proto::cbor::decode::<ByKind>(&bytes) {
                Ok(m) => m,
                Err(e) => {
                    let aside = path.with_extension(format!("cbor.corrupt-{}", super::now_ms()));
                    match std::fs::rename(&path, &aside) {
                        Ok(()) => eprintln!("models cache {}: {e}; moved to {}, starting with no models listed", path.display(), aside.display()),
                        Err(r) => eprintln!("models cache {}: {e}; moving it to {} failed too ({r})", path.display(), aside.display()),
                    }
                    ByKind::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ByKind::new(),
            Err(e) => {
                eprintln!("models cache {}: reading it: {e}; starting with no models listed", path.display());
                ByKind::new()
            }
        };
        Self { path, by_kind: Arc::new(by_kind) }
    }

    /// Every kind's list, as kept now.
    pub fn snapshot(&self) -> Arc<ByKind> {
        self.by_kind.clone()
    }

    /// Kind `kind` listed `models`: kept, and written through when it differs from what was kept. Whether it changed.
    ///
    /// # Errors
    /// The file's IO (the list is still kept in memory), naming the path.
    pub fn learn(&mut self, kind: &str, models: &[ModelChoice]) -> anyhow::Result<bool> {
        if kind.is_empty() || models.is_empty() || self.by_kind.get(kind).is_some_and(|m| m == models) {
            return Ok(false);
        }
        Arc::make_mut(&mut self.by_kind).insert(kind.to_owned(), models.to_vec());
        write_atomic(&self.path, &harness_proto::cbor::encode(&*self.by_kind))?;
        Ok(true)
    }
}

/// `bytes` at `path`, whole or not at all: written beside, synced, renamed over, the directory synced.
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context;
    let tmp = path.with_extension("cbor.tmp");
    let mut f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(bytes).with_context(|| format!("writing {}", tmp.display()))?;
    f.sync_all().with_context(|| format!("fsync {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming {} over {}", tmp.display(), path.display()))?;
    if let Some(dir) = path.parent() {
        File::open(dir).and_then(|d| d.sync_all()).with_context(|| format!("fsync {}", dir.display()))?;
    }
    Ok(())
}

/// The daemon's cache, at `$HARNESS_HOME/models.cbor`, restored on first use (the daemon touches it as it starts).
static CACHE: std::sync::LazyLock<Mutex<Cache>> = std::sync::LazyLock::new(|| Mutex::new(Cache::restore(crate::paths::home().join(FILE))));

/// Restore the daemon's cache now (at start, so a corrupt file is reported then).
pub fn restore() {
    let n = CACHE.lock().snapshot().len();
    if n > 0 {
        eprintln!("models cache: {n} agent kind(s) listed before");
    }
}

/// Every kind's list, as the daemon keeps it.
/// Whether kind `kind` can start on `model` at `effort` (a spawn's): each named must be in the list it last gave (the
/// model by [`ModelChoice::is`], the effort among that model's). A kind that never listed (Cursor) passes its model to
/// the agent unchecked, which refuses it itself; an effort needs the model's entry to be known.
///
/// # Errors
/// Naming the model or effort and what the list has instead.
pub fn check(kind: &str, model: Option<&str>, effort: Option<&str>) -> anyhow::Result<()> {
    let all = snapshot();
    let Some(list) = all.get(kind) else {
        anyhow::ensure!(effort.is_none(), "{kind} has listed no models, so effort {effort:?} is not known to be one it takes");
        return Ok(());
    };
    let ids = || list.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(", ");
    let entry = match model {
        Some(m) => Some(list.iter().find(|c| c.is(m)).ok_or_else(|| anyhow::anyhow!("{kind} does not offer model {m:?} (it lists {})", ids()))?),
        None => None,
    };
    if let Some(e) = effort {
        let Some(c) = entry else { anyhow::bail!("effort {e:?} for {kind} names no model, so it is not known to take it") };
        anyhow::ensure!(c.efforts.iter().any(|x| x == e), "{kind}'s {} takes efforts {:?}, not {e:?}", c.id, c.efforts);
    }
    Ok(())
}

pub fn snapshot() -> Arc<ByKind> {
    CACHE.lock().snapshot()
}

/// Kind `kind`'s agent listed `models` (a `models` event it wrote): kept, and `list` clients told when it changed.
pub fn learned(kind: &str, models: &[ModelChoice]) {
    match CACHE.lock().learn(kind, models) {
        Ok(true) => crate::daemon::changed(),
        Ok(false) => {}
        Err(e) => eprintln!("models cache: keeping {kind}'s models: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("harness-models-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn opus() -> Vec<ModelChoice> {
        vec![
            ModelChoice { id: "opus".into(), model: "claude-opus-5-5".into(), name: "Opus 5.5".into(), efforts: vec!["low".into(), "high".into()], ..ModelChoice::default() },
            ModelChoice { id: "haiku".into(), model: "claude-haiku-5".into(), name: "Haiku 5".into(), ..ModelChoice::default() },
        ]
    }

    /// persisted-state-round-trips: what a daemon learned, a fresh daemon restores equal, and its file is byte for byte
    /// the encoding a fresh cache of the same lists writes; learning the same list again writes nothing new.
    #[test]
    fn the_models_cache_round_trips() {
        let d = dir("roundtrip");
        let path = d.join(FILE);
        let mut c = Cache::restore(path.clone());
        assert!(c.snapshot().is_empty(), "no file: nothing listed");
        assert!(c.learn("claude", &opus()).unwrap());
        assert!(c.learn("codex", &[ModelChoice { id: "gpt-5.6".into(), effort: Some("medium".into()), ..ModelChoice::default() }]).unwrap());
        assert!(!c.learn("claude", &opus()).unwrap(), "the same list again is no change");
        let back = Cache::restore(path.clone());
        assert_eq!(back.snapshot(), c.snapshot(), "restored equal");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, harness_proto::cbor::encode(&*back.snapshot()), "the file is the restored lists' encoding");
        let fresh = d.join("fresh.cbor");
        let mut f = Cache::restore(fresh.clone());
        f.learn("codex", &back.snapshot()["codex"]).unwrap();
        f.learn("claude", &back.snapshot()["claude"]).unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), bytes, "a fresh cache of the same lists writes the same bytes");
        assert!(!d.join("models.cbor.tmp").exists(), "no temp file left");
    }

    /// A file that does not decode is moved aside, never read as empty in its place: the next write does not lose it.
    #[test]
    fn a_corrupt_models_cache_is_moved_aside() {
        let d = dir("corrupt");
        let path = d.join(FILE);
        std::fs::write(&path, b"\xff not cbor").unwrap();
        let mut c = Cache::restore(path.clone());
        assert!(c.snapshot().is_empty());
        assert!(!path.exists(), "moved aside");
        let aside: Vec<_> = std::fs::read_dir(&d).unwrap().filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(aside.iter().any(|n| n.starts_with("models.cbor.corrupt-")), "kept as {aside:?}");
        c.learn("claude", &opus()).unwrap();
        assert_eq!(Cache::restore(path).snapshot().get("claude"), Some(&opus()));
    }
}
