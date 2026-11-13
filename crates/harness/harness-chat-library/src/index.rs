//! The local chat search index. One worker owns the connection; a source replacement and its FTS postings commit
//! together. Refresh commits one source at a time, keeping memory bounded and recent results available while the
//! first scan continues. Source discovery/parsing is the driver's IO; `refresh_sources` also takes injected sources
//! and a reader, so the same index decisions run without files or a clock in the seeded tests.
use crate::{Parsed, Roots, Session, Source};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SCHEMA: i64 = 1;
const APP_ID: i64 = 0x4843_4c31;
const MAX_HITS: usize = 200;
const MAX_QUERY_CHARS: usize = 512;
const PROGRESS_EVERY: usize = 16;

// The winning source for a provider conversation is Harness's own chat, then the most recently changed source;
// path breaks ties deterministically. A source without a provider chat id has its own key as its identity.
const VISIBLE: &str = "NOT EXISTS (
    SELECT 1 FROM documents other WHERE other.identity = d.identity AND (
        other.harness > d.harness OR (other.harness = d.harness AND (
            other.updated_ms > d.updated_ms OR (other.updated_ms = d.updated_ms AND other.path < d.path)
        ))
    )
)";

/// A ranked match. The transcript is read only when this result is opened.
#[derive(Clone, Debug)]
pub struct Hit {
    pub session: Session,
    pub snippet: String,
}

/// SQLite's atomic transactions are the persistence boundary; no custom journal or recovery format is added.
/// The schema and parser generation are explicit, so another layout is refused instead of read as an empty cache.
pub struct Index {
    db: Connection,
    path: PathBuf,
    diagnostics: Vec<String>,
    #[cfg(test)]
    plant: Plant,
}

#[cfg(test)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Plant {
    #[default]
    None,
    KeepFailed,
    KeepRemoved,
}

impl Index {
    /// Open the index at `path`. A corrupt or differently versioned index is an error naming that file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let exists = match std::fs::metadata(path) {
            Ok(m) if m.len() == 0 => return Err(format!("chat index {} already exists but is empty; refusing to treat a lost index as a fresh one", path.display())),
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(format!("chat index {}: {e}", path.display())),
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| format!("chat index directory {}: {e}", parent.display()))?;
        }
        if !exists && path != Path::new(":memory:") {
            let mut file = std::fs::OpenOptions::new();
            file.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                file.mode(0o600);
            }
            file.open(path).map_err(|e| format!("creating private chat index {}: {e}", path.display()))?;
        }
        let db = Connection::open(path).map_err(|e| format!("chat index {}: {e}", path.display()))?;
        Self::from_connection(db, path.to_path_buf())
    }

    fn from_connection(mut db: Connection, path: PathBuf) -> Result<Self, String> {
        let error = |e| format!("chat index {}: {e}", path.display());
        // A second writer is reported immediately. Waiting and retrying an index write would stall its search worker.
        db.busy_timeout(Duration::ZERO).map_err(error)?;
        db.execute_batch("PRAGMA mmap_size = 0; PRAGMA foreign_keys = ON; PRAGMA case_sensitive_like = ON;")
            .map_err(error)?;
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0)).map_err(error)?;
        let app: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0)).map_err(error)?;
        if version == 0 && app == 0 {
            let tables: i64 = db.query_row("SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'", [], |r| r.get(0)).map_err(error)?;
            if tables != 0 {
                return Err(format!("chat index {} has no Harness schema version but already contains {tables} objects", path.display()));
            }
            let tx = db.transaction().map_err(error)?;
            tx.execute_batch(
                "CREATE TABLE documents (
                    rowid INTEGER PRIMARY KEY,
                    path TEXT NOT NULL UNIQUE,
                    source_agent TEXT NOT NULL,
                    modified_ms INTEGER NOT NULL,
                    len INTEGER NOT NULL,
                    session_key TEXT NOT NULL,
                    key_fold TEXT NOT NULL,
                    chat_id TEXT NOT NULL,
                    id_fold TEXT NOT NULL,
                    title TEXT NOT NULL,
                    title_fold TEXT NOT NULL,
                    identity TEXT NOT NULL,
                    updated_ms INTEGER NOT NULL,
                    harness INTEGER NOT NULL,
                    session BLOB NOT NULL,
                    body TEXT NOT NULL
                );
                CREATE INDEX documents_key ON documents(key_fold);
                CREATE INDEX documents_session ON documents(session_key);
                CREATE INDEX documents_id ON documents(id_fold);
                CREATE INDEX documents_title ON documents(title_fold);
                CREATE INDEX documents_identity ON documents(identity, harness, updated_ms, path);
                CREATE INDEX documents_recent ON documents(updated_ms DESC, session_key);
                CREATE VIRTUAL TABLE documents_fts USING fts5(
                    title, chat_id, body, content='documents', content_rowid='rowid',
                    tokenize='unicode61 remove_diacritics 2'
                );
                CREATE TRIGGER documents_insert AFTER INSERT ON documents BEGIN
                    INSERT INTO documents_fts(rowid,title,chat_id,body) VALUES(new.rowid,new.title,new.chat_id,new.body);
                END;
                CREATE TRIGGER documents_delete AFTER DELETE ON documents BEGIN
                    INSERT INTO documents_fts(documents_fts,rowid,title,chat_id,body)
                        VALUES('delete',old.rowid,old.title,old.chat_id,old.body);
                END;
                CREATE TRIGGER documents_update AFTER UPDATE OF title,chat_id,body ON documents BEGIN
                    INSERT INTO documents_fts(documents_fts,rowid,title,chat_id,body)
                        VALUES('delete',old.rowid,old.title,old.chat_id,old.body);
                    INSERT INTO documents_fts(rowid,title,chat_id,body) VALUES(new.rowid,new.title,new.chat_id,new.body);
                END;
                PRAGMA application_id = 1212369969;
                PRAGMA user_version = 1;",
            ).map_err(error)?;
            tx.commit().map_err(error)?;
        } else if version != SCHEMA || app != APP_ID {
            return Err(format!("chat index {} uses application {app:#x}, schema {version}; this Harness reads application {APP_ID:#x}, schema {SCHEMA}", path.display()));
        }
        db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;").map_err(error)?;
        // Prepare the actual reader now, so a damaged/missing table fails at open, not as an empty search.
        db.prepare("SELECT session FROM documents LIMIT 0").map_err(error)?;
        Ok(Self { db, path, diagnostics: Vec::new(), #[cfg(test)] plant: Plant::None })
    }

    /// Refresh changed sources, preserving successfully committed sources if a later one cannot be read.
    pub fn refresh(&mut self, roots: &Roots) -> Result<usize, String> {
        self.refresh_with_progress(roots, |_, _, _| {})
    }

    /// Refresh newest sources first. The callback runs initially, every sixteen sources and at completion; it can
    /// query the committed index to answer UI requests without waiting for the whole corpus to finish.
    pub fn refresh_with_progress(&mut self, roots: &Roots, progress: impl FnMut(&Self, usize, usize)) -> Result<usize, String> {
        self.diagnostics.clear();
        let sources = crate::discover(roots)?;
        let metadata = crate::metadata(roots)?;
        self.patch_metadata(&metadata)?;
        self.refresh_sources(sources, |source| {
            let mut parsed = crate::read(source)?;
            crate::apply_metadata(&mut parsed.session, &metadata);
            Ok(parsed)
        }, progress)
    }

    // A rename/model change in a provider's metadata store does not mean its unchanged multi-megabyte transcript
    // needs parsing again. Patch only affected descriptors, letting the title trigger update FTS when necessary.
    fn patch_metadata(&mut self, metadata: &BTreeMap<String, crate::SessionMetadata>) -> Result<(), String> {
        let mut patched = BTreeSet::new(); // alloc: descriptors matched by both their key and provider identity
        for key in metadata.keys() {
            let rows: Vec<(String, Vec<u8>)> = {
                let mut query = self.db.prepare("SELECT path,session FROM documents WHERE session_key=?1 OR identity=?1")
                    .map_err(|e| self.error(e))?;
                query.query_map([key], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|e| self.error(e))?
                    .collect::<rusqlite::Result<_>>().map_err(|e| self.error(e))?
            };
            for (path, bytes) in rows {
                if !patched.insert(path.clone()) {
                    continue;
                }
                let old: Session = harness_proto::cbor::decode(&bytes).map_err(|e| format!("chat index {} source {path}: metadata record: {e}", self.path.display()))?;
                let mut session = old.clone();
                crate::apply_metadata(&mut session, metadata);
                if old == session {
                    continue;
                }
                let updated = sql_u64(session.updated_ms, "chat update time", &path)?;
                let payload = harness_proto::cbor::encode(&session);
                let context = format!("chat index {} updating metadata for {path}", self.path.display());
                let tx = self.db.transaction().map_err(|e| format!("{context}: {e}"))?;
                if old.title != session.title {
                    tx.execute("UPDATE documents SET session=?1,updated_ms=?2,title=?3,title_fold=?4 WHERE path=?5",
                        params![payload,updated,session.title,session.title.to_lowercase(),path])
                        .map_err(|e| format!("{context}: {e}"))?;
                } else {
                    tx.execute("UPDATE documents SET session=?1,updated_ms=?2 WHERE path=?3", params![payload,updated,path])
                        .map_err(|e| format!("{context}: {e}"))?;
                }
                tx.commit().map_err(|e| format!("{context}: {e}"))?;
            }
        }
        Ok(())
    }

    fn refresh_sources(
        &mut self,
        mut sources: Vec<Source>,
        mut read: impl FnMut(&Source) -> Result<Parsed, String>,
        mut progress: impl FnMut(&Self, usize, usize),
    ) -> Result<usize, String> {
        self.diagnostics.clear();
        sources.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms).then_with(|| a.path.cmp(&b.path)));
        let mut old = self.stamps()?;
        let total = sources.len();
        let mut changed = 0;
        progress(self, 0, total);
        for (at, source) in sources.into_iter().enumerate() {
            let key = path_text(&source.path)?;
            let stamp = (source.agent.clone(), source.modified_ms, source.len); // alloc: one source's stamp
            if old.remove(&key).as_ref() != Some(&stamp) {
                match read(&source) {
                    Ok(parsed) => {
                        self.replace(&source, &parsed, false)?;
                        changed += 1;
                    }
                    Err(error) => {
                        // This source is explicitly failed, not answered from an older indexed copy. Other sources
                        // continue, and the UI can report every named failure alongside successful results.
                        if !self.keep_failed() {
                            self.db.execute("DELETE FROM documents WHERE path=?1", [&key]).map_err(|e| self.error(e))?;
                        }
                        self.diagnostics.push(format!("{}: {error}", source.path.display()));
                    }
                }
            }
            let done = at + 1;
            if done % PROGRESS_EVERY == 0 || done == total {
                progress(self, done, total);
            }
        }
        if !old.is_empty() && !self.keep_removed() {
            let context = format!("chat index {} removing disappeared sources", self.path.display());
            let tx = self.db.transaction().map_err(|e| format!("{context}: {e}"))?;
            for path in old.keys() {
                tx.execute("DELETE FROM documents WHERE path=?1", [path]).map_err(|e| format!("{context}, {path}: {e}"))?;
            }
            tx.commit().map_err(|e| format!("{context}: {e}"))?;
            progress(self, total, total);
        }
        if self.diagnostics.is_empty() {
            Ok(changed)
        } else {
            Err(format!("chat index {}: {} transcript source(s) failed; {}\n{}", self.path.display(), self.diagnostics.len(),
                "their stale entries were removed; other conversations are indexed", self.diagnostics.iter().take(8).cloned().collect::<Vec<_>>().join("\n")))
        }
    }

    /// Named per-source failures from the last refresh. Search results exclude these sources' previous contents.
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    fn keep_failed(&self) -> bool {
        #[cfg(test)]
        { self.plant == Plant::KeepFailed }
        #[cfg(not(test))]
        { false }
    }

    fn keep_removed(&self) -> bool {
        #[cfg(test)]
        { self.plant == Plant::KeepRemoved }
        #[cfg(not(test))]
        { false }
    }

    fn stamps(&self) -> Result<BTreeMap<String, (String, u64, u64)>, String> {
        let mut q = self.db.prepare("SELECT path,source_agent,modified_ms,len FROM documents").map_err(|e| self.error(e))?;
        let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)))
            .map_err(|e| self.error(e))?;
        let mut stamps = BTreeMap::new(); // alloc: one metadata stamp per source in this refresh
        for row in rows {
            let (path, agent, modified, len) = row.map_err(|e| self.error(e))?;
            let modified = u64::try_from(modified).map_err(|_| self.error(format!("source {path}: stored modification time {modified} is negative")))?;
            let len = u64::try_from(len).map_err(|_| self.error(format!("source {path}: stored length {len} is negative")))?;
            stamps.insert(path, (agent, modified, len));
        }
        Ok(stamps)
    }

    // `fail_before_commit` is used only by the simulation to inject a failed transaction after all its writes.
    fn replace(&mut self, source: &Source, parsed: &Parsed, fail_before_commit: bool) -> Result<(), String> {
        let s = &parsed.session;
        let path = path_text(&source.path)?;
        if s.path != source.path {
            return Err(format!("chat index source {path}: parser returned different path {}", s.path.display()));
        }
        let modified = sql_u64(source.modified_ms, "source modification time", &path)?;
        let len = sql_u64(source.len, "source length", &path)?;
        let updated = sql_u64(s.updated_ms, "chat update time", &path)?;
        let identity = if s.chat_id.is_empty() { s.key.clone() } else { format!("{}:{}", s.agent, s.chat_id) };
        let payload = harness_proto::cbor::encode(s);
        let context = format!("chat index {} replacing {path}", self.path.display());
        let tx = self.db.transaction().map_err(|e| format!("{context}: {e}"))?;
        tx.execute(
            "INSERT INTO documents(path,source_agent,modified_ms,len,session_key,key_fold,chat_id,id_fold,title,title_fold,
                identity,updated_ms,harness,session,body) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
             ON CONFLICT(path) DO UPDATE SET source_agent=excluded.source_agent,modified_ms=excluded.modified_ms,
                len=excluded.len,session_key=excluded.session_key,key_fold=excluded.key_fold,chat_id=excluded.chat_id,
                id_fold=excluded.id_fold,title=excluded.title,title_fold=excluded.title_fold,identity=excluded.identity,
                updated_ms=excluded.updated_ms,harness=excluded.harness,session=excluded.session,body=excluded.body",
            params![path, source.agent, modified, len, s.key, s.key.to_lowercase(), s.chat_id, s.chat_id.to_lowercase(),
                s.title, s.title.to_lowercase(), identity, updated, i64::from(s.harness_id.is_some()), payload, parsed.text],
        ).map_err(|e| format!("{context}: {e}"))?;
        if fail_before_commit {
            return Err(format!("{context}: injected transaction failure before commit"));
        }
        tx.commit().map_err(|e| format!("{context}: {e}"))
    }

    /// Exact id/title, then id/title prefix, title token matches, then weighted BM25 over the conversation.
    /// Typed punctuation is treated as text, never as an FTS expression or SQL.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, String> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(format!("chat search query exceeds {MAX_QUERY_CHARS} characters"));
        }
        let limit = limit.min(MAX_HITS);
        let query = query.trim().to_lowercase();
        let mut out = Vec::with_capacity(limit); // alloc: returned search result page
        let mut seen = BTreeSet::new(); // alloc: one page's keys, preserving SQL rank across stages
        if query.is_empty() {
            let sql = format!("SELECT d.session, substr(d.body,1,1600) FROM documents d WHERE {VISIBLE}
                ORDER BY d.updated_ms DESC,d.session_key,d.path LIMIT ?1");
            let mut q = self.db.prepare(&sql).map_err(|e| self.error(e))?;
            let rows = q.query_map([limit as i64], row_hit).map_err(|e| self.error(e))?;
            for row in rows {
                append_hit(&mut out, &mut seen, row.map_err(|e| self.error(e))?).map_err(|e| self.error(e))?;
            }
            return Ok(out);
        }
        let prefix = format!("{}%", like_literal(&query));
        let sql = format!("SELECT d.session,substr(d.body,1,1600) FROM documents d WHERE {VISIBLE} AND (
                d.id_fold=?1 OR d.key_fold=?1 OR d.title_fold=?1 OR
                d.id_fold LIKE ?2 ESCAPE '\\' OR d.key_fold LIKE ?2 ESCAPE '\\' OR d.title_fold LIKE ?2 ESCAPE '\\'
            ) ORDER BY CASE WHEN d.id_fold=?1 OR d.key_fold=?1 OR d.title_fold=?1 THEN 0 ELSE 1 END,
                d.updated_ms DESC,d.session_key,d.path LIMIT ?3");
        let mut q = self.db.prepare(&sql).map_err(|e| self.error(e))?;
        let rows = q.query_map(params![query, prefix, limit as i64], row_hit).map_err(|e| self.error(e))?;
        for row in rows {
            append_hit(&mut out, &mut seen, row.map_err(|e| self.error(e))?).map_err(|e| self.error(e))?;
        }
        let Some(expression) = fts_terms(&query) else { return Ok(out) };
        for title_only in [true, false] {
            if out.len() >= limit {
                break;
            }
            let expression = if title_only { format!("title : ({expression})") } else { expression.clone() };
            let sql = format!("SELECT d.session,snippet(documents_fts,2,'','',' … ',80)
                FROM documents_fts JOIN documents d ON d.rowid=documents_fts.rowid
                WHERE documents_fts MATCH ?1 AND {VISIBLE}
                ORDER BY bm25(documents_fts,12.0,16.0,1.0),d.updated_ms DESC,d.session_key,d.path LIMIT ?2");
            let mut q = self.db.prepare(&sql).map_err(|e| self.error(e))?;
            let rows = q.query_map(params![expression, limit as i64], row_hit).map_err(|e| self.error(e))?;
            for row in rows {
                if out.len() >= limit {
                    break;
                }
                append_hit(&mut out, &mut seen, row.map_err(|e| self.error(e))?).map_err(|e| self.error(e))?;
            }
        }
        Ok(out)
    }

    /// Read a selected chat from its original source, with current contents. The index never owns source logs.
    pub fn read(&self, key: &str) -> Result<Parsed, String> {
        let sql = format!("SELECT d.session FROM documents d WHERE d.session_key=?1 AND {VISIBLE} LIMIT 1");
        let payload: Option<Vec<u8>> = self.db.query_row(&sql, [key], |r| r.get(0)).optional().map_err(|e| self.error(e))?;
        let payload = payload.ok_or_else(|| format!("chat index {} has no conversation {key:?}; refresh the chat list", self.path.display()))?;
        let s: Session = harness_proto::cbor::decode(&payload).map_err(|e| format!("chat index {} conversation {key:?}: {e}", self.path.display()))?;
        let parsed = crate::read_session(&s)?;
        if parsed.session.key != s.key {
            return Err(format!("chat source {} changed identity from {:?} to {:?}; refresh the chat list", s.path.display(), s.key, parsed.session.key));
        }
        Ok(parsed)
    }

    fn error(&self, error: impl std::fmt::Display) -> String {
        format!("chat index {}: {error}", self.path.display())
    }
}

fn path_text(path: &Path) -> Result<String, String> {
    path.to_str().map(str::to_owned).ok_or_else(|| format!("chat index source path {path:?} is not UTF-8"))
}

fn sql_u64(n: u64, what: &str, path: &str) -> Result<i64, String> {
    i64::try_from(n).map_err(|_| format!("chat index source {path}: {what} {n} exceeds SQLite's integer range"))
}

fn row_hit(row: &Row<'_>) -> rusqlite::Result<(Vec<u8>, String)> {
    Ok((row.get(0)?, row.get(1)?))
}

fn append_hit(out: &mut Vec<Hit>, seen: &mut BTreeSet<String>, (bytes, snippet): (Vec<u8>, String)) -> Result<(), String> {
    let session: Session = harness_proto::cbor::decode(&bytes).map_err(|e| format!("decoding a search result's session record: {e}"))?;
    if seen.insert(session.key.clone()) {
        let snippet = snippet.trim().chars().take(1600).collect(); // alloc: bounded result excerpt
        out.push(Hit { session, snippet });
    }
    Ok(())
}

fn like_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len()); // alloc: one bound SQL prefix
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Every token is quoted and the final token is a prefix, so ordinary partial typing finds a word without accepting
/// FTS operators from the person. Punctuation-only input still uses the literal id/title stages.
fn fts_terms(query: &str) -> Option<String> {
    let mut terms = query.split(|c: char| !c.is_alphanumeric()).filter(|s| !s.is_empty()).peekable();
    let mut out = String::new(); // alloc: one bound FTS query
    while let Some(term) = terms.next() {
        if !out.is_empty() {
            out.push_str(" AND ");
        }
        out.push('"');
        out.push_str(term);
        out.push('"');
        if terms.peek().is_none() {
            out.push('*');
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests;
