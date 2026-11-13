use super::*;
use hegel::generators as gs;

fn memory() -> Index {
    Index::from_connection(Connection::open_in_memory().unwrap(), PathBuf::from("<simulated chat index>")).unwrap()
}

fn document(n: u64, title: &str, text: &str, updated: u64) -> (Source, Parsed) {
    let path = PathBuf::from(format!("/sim/claude-{n}.jsonl"));
    let source = Source { path: path.clone(), agent: "claude".into(), modified_ms: updated, len: text.len() as u64, harness_id: None };
    let session = Session {
        key: format!("claude:chat-{n}"), agent: "claude".into(), chat_id: format!("chat-{n}"), title: title.into(),
        cwd: "/sim/project".into(), model: "claude-fable-5-2".into(), effort: "high".into(), updated_ms: updated, path,
        harness_id: None,
    };
    (source, Parsed { session, events: Vec::new(), text: text.into() })
}

fn put(index: &mut Index, n: u64, title: &str, text: &str, updated: u64) {
    let (source, parsed) = document(n, title, text, updated);
    index.replace(&source, &parsed, false).unwrap();
}

fn keys(index: &Index, query: &str) -> Vec<String> {
    index.search(query, 100).unwrap().into_iter().map(|h| h.session.key).collect()
}

#[test]
fn exact_identity_and_titles_beat_title_tokens_and_body_bm25() {
    let mut index = memory();
    put(&mut index, 1, "cache", "A short description", 1);
    put(&mut index, 2, "cache cleanup", "A short description", 2);
    put(&mut index, 3, "Investigate the cache", "A short description", 3);
    put(&mut index, 4, "Ordinary conversation", "cache cache cache cache cache", 4);
    assert_eq!(keys(&index, "cache"), ["claude:chat-1", "claude:chat-2", "claude:chat-3", "claude:chat-4"]);
    put(&mut index, 5, "Mentions an identity", "chat-1 chat-1 chat-1", 5);
    assert_eq!(keys(&index, "chat-1")[0], "claude:chat-1");
    assert_eq!(keys(&index, "cache cleanup"), ["claude:chat-2"]);
    assert!(keys(&index, "nevermentioned").is_empty());
    assert_eq!(keys(&index, "")[0], "claude:chat-5");
    assert!(index.search("cache", 0).unwrap().is_empty());
    let markdown = "## Rendered excerpt\n\nKeep **cache** entries and `code` intact.\n\n- Preserve paragraphs\n- Preserve lists";
    put(&mut index, 6, "Preview formatting", markdown, 6);
    for query in ["", "chat-6", "rendered"] {
        let hits = index.search(query, 100).unwrap();
        let hit = hits.iter().find(|h| h.session.chat_id == "chat-6").unwrap();
        assert_eq!(hit.snippet, markdown, "all query stages preserve the excerpt's Markdown structure");
    }
}

#[test]
fn typed_fts_and_like_punctuation_are_text_not_operators() {
    let mut index = memory();
    put(&mut index, 1, "literal_%", "quoted words", 1);
    put(&mut index, 2, "literal wildcard", "other content", 2);
    assert_eq!(keys(&index, "literal_%")[0], "claude:chat-1");
    for q in ["\"", "*", "a:b", "cache OR other", "NOT", "(", "\"word\"*", "_\\%", "界面"] {
        assert!(index.search(q, 20).is_ok(), "typed input {q:?} is never parsed as an FTS expression");
    }
}

#[test]
fn provider_renames_patch_search_without_reparsing_unchanged_transcripts() {
    let mut index = memory();
    put(&mut index, 1, "Original title", "conversation body", 1);
    let metadata = BTreeMap::from([("claude:chat-1".to_owned(), crate::SessionMetadata {
        title: "Renamed conversation".into(), effort: "max".into(), updated_ms: 3, ..Default::default()
    })]);
    index.patch_metadata(&metadata).unwrap();
    assert!(keys(&index, "Original").is_empty());
    let result = index.search("Renamed", 1).unwrap();
    assert_eq!(result[0].session.effort, "max");
    assert_eq!(result[0].session.updated_ms, 3);
    let source = document(1, "Original title", "conversation body", 1).0;
    index.refresh_sources(vec![source], |_| panic!("metadata changes must not reread unchanged transcript bytes"), |_,_,_| {}).unwrap();
    assert_eq!(keys(&index, "Renamed"), ["claude:chat-1"]);
}

/// The invariant: one unreadable source is reported and loses its old searchable contents, while other source
/// updates commit and remain searchable. The schedule chooses which source fails; all IO is injected.
#[hegel::test(test_cases = 16)]
fn a_failed_source_does_not_hide_other_chats_or_serve_stale_text(tc: hegel::TestCase) {
    let fail: u64 = tc.draw(gs::integers().max_value(2));
    failed_source(fail, Plant::None).unwrap();
}

fn failed_source(fail: u64, plant: Plant) -> Result<(), String> {
    let mut index = memory();
    index.plant = plant;
    for n in 0..3 {
        put(&mut index, n, &format!("oldtitle{n}"), &format!("stale{n}"), 1);
    }
    let sources = (0..3).map(|n| document(n, &format!("newtitle{n}"), &format!("fresh{n}"), 2).0).collect();
    let error = index.refresh_sources(sources, |source| {
        let n: u64 = source.path.file_stem().unwrap().to_str().unwrap().strip_prefix("claude-").unwrap().parse().unwrap();
        if n == fail {
            Err("line 2 is not a complete JSON record".to_owned())
        } else {
            Ok(document(n, &format!("newtitle{n}"), &format!("fresh{n}"), 2).1)
        }
    }, |_,_,_| {}).unwrap_err();
    assert!(error.contains("1 transcript source(s) failed"));
    assert_eq!(index.diagnostics().len(), 1);
    assert!(index.diagnostics()[0].contains(&format!("claude-{fail}.jsonl")));
    for n in 0..3 {
        if !keys(&index, &format!("stale{n}")).is_empty() || keys(&index, &format!("fresh{n}")).len() != usize::from(n != fail) {
            return Err(format!("source {n} retained stale text or hid another successful source"));
        }
    }
    Ok(())
}

#[test]
fn the_oracle_catches_a_failed_source_answered_from_stale_text() {
    assert!(failed_source(1, Plant::KeepFailed).is_err());
}

#[test]
fn harness_owns_a_conversation_when_an_external_copy_is_also_indexed() {
    let mut index = memory();
    put(&mut index, 1, "External title", "same topic", 90);
    let (mut source, mut parsed) = document(1, "Harness title", "same topic", 80);
    source.path = PathBuf::from("/sim/objects/s7/chat");
    source.agent = "harness".into();
    source.harness_id = Some(7);
    parsed.session.path = source.path.clone();
    parsed.session.key = "harness:s7".into();
    parsed.session.harness_id = Some(7);
    index.replace(&source, &parsed, false).unwrap();
    assert_eq!(keys(&index, "chat-1"), ["harness:s7"]);
    assert_eq!(keys(&index, "same topic"), ["harness:s7"]);
    assert_eq!(index.search("", 20).unwrap()[0].session.harness_id, Some(7));
    index.refresh_sources(vec![document(1, "External title", "same topic", 90).0], |s| {
        assert_eq!(s.agent, "claude");
        panic!("the unchanged external source must not be reparsed")
    }, |_, _, _| {}).unwrap();
    assert_eq!(keys(&index, "chat-1"), ["claude:chat-1"], "removing the Harness source reveals the external copy");
}

fn snapshot(index: &mut Index) -> Vec<u8> {
    index.db.serialize(rusqlite::MAIN_DB).unwrap().to_vec()
}

fn restore(bytes: &[u8]) -> Index {
    let mut db = Connection::open_in_memory().unwrap();
    db.deserialize_read_exact(rusqlite::MAIN_DB, bytes, bytes.len(), false).unwrap();
    Index::from_connection(db, PathBuf::from("<restored simulated chat index>")).unwrap()
}

/// The invariant: a committed source replacement is visible exactly once, a failed transaction changes nothing,
/// disappeared sources leave no matches, and restoring a committed SQLite image returns exactly the same sessions
/// and rankings as a fresh index of the source snapshot. SQLite's transaction/serialization boundary is trusted;
/// this simulation tests our replacement/deletion decisions without filesystem, sleeps or an OS process.
#[hegel::test(test_cases = 32)]
fn incremental_refresh_and_reopen_equal_a_fresh_index(tc: hegel::TestCase) {
    let seed: u64 = tc.draw(gs::integers());
    simulate(seed, false).unwrap();
}

fn simulate(mut seed: u64, keep_removed_source: bool) -> Result<(), String> {
    let mut index = memory();
    let mut expected: BTreeMap<u64, (String, String, u64)> = BTreeMap::new();
    for revision in 1..=40 {
        // A fixed seeded schedule is independent of wall time, SQL rowids and hash iteration order.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let n = seed % 7;
        if seed % 5 == 0 {
            expected.remove(&n);
        } else {
            expected.insert(n, (format!("topic {n}"), format!("needle{n} revision {revision}"), revision));
        }
        let sources: Vec<Source> = expected.iter().map(|(&n, (title, text, updated))| document(n, title, text, *updated).0).collect();
        index.plant = if keep_removed_source { Plant::KeepRemoved } else { Plant::None };
        index.refresh_sources(sources, |s| {
            let n: u64 = s.path.file_stem().unwrap().to_str().unwrap().strip_prefix("claude-").unwrap().parse().unwrap();
            let (title, text, updated) = &expected[&n];
            Ok(document(n, title, text, *updated).1)
        }, |_, _, _| {}).unwrap();
        // Inject a failed write after FTS and metadata changed but before commit. Drop must roll back both.
        let before = keys(&index, "");
        let (s, p) = document(99, "uncommitted", "notvisible", revision);
        assert!(index.replace(&s, &p, true).is_err());
        assert_eq!(keys(&index, ""), before);
        assert!(keys(&index, "notvisible").is_empty());
        let bytes = snapshot(&mut index);
        index = restore(&bytes);
        let mut fresh = memory();
        for (&n, (title, text, updated)) in &expected {
            let (s, p) = document(n, title, text, *updated);
            fresh.replace(&s, &p, false).unwrap();
        }
        for query in ["", "topic", "needle0", "needle1", "chat-1"] {
            let actual = index.search(query, 100).unwrap();
            let reference = fresh.search(query, 100).unwrap();
            let sessions = |hits: Vec<Hit>| hits.into_iter().map(|h| h.session).collect::<Vec<_>>();
            if sessions(actual) != sessions(reference) {
                return Err(format!("source snapshot disagrees at revision {revision}, query {query:?}"));
            }
        }
    }
    Ok(())
}

#[test]
fn the_oracle_catches_a_removed_source_kept_in_the_index() {
    assert!(simulate(0x1234_5678_9abc_def0, true).is_err());
}

#[test]
fn another_schema_and_corrupt_metadata_are_refused_by_name() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("PRAGMA user_version=9; PRAGMA application_id=1212369969;").unwrap();
    let error = match Index::from_connection(db, PathBuf::from("old-chat-index.sqlite")) {
        Ok(_) => panic!("another schema must be refused"),
        Err(e) => e,
    };
    assert!(error.contains("old-chat-index.sqlite") && error.contains("schema 9"));
    let mut index = memory();
    put(&mut index, 1, "title", "body", 1);
    index.db.execute("UPDATE documents SET session=?1", [b"not cbor".as_slice()]).unwrap();
    let error = index.search("", 20).unwrap_err();
    assert!(error.contains("simulated chat index") && error.contains("session record"));
}
