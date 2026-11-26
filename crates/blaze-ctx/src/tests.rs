//! Comprehensive unit tests for `blaze-ctx`:
//! - Identifier-aware tokenization (`snake_case`, `CamelCase`, `HTTPServer`, `kebab-case`, `::`)
//! - BM25 + character 3-gram scoring (morphological variants, status freshness)
//! - 1-hop typed concept graph expansion (`crate` <-> `design doc` <-> `annoyance`)
//! - Dense `EmbeddingStore` cosine similarity, hybrid fusion, and strict error refusal
//! - Token-budget truncation & lossless `blaze-tab` round-trip/formatting
//! - Incremental BLAKE3 cache reuse & deterministic `blaze-cbor` round-trip + schema version refusal

use blaze_own::{AString, Alloc};

use crate::{
    CacheError, CrateSnapshot, CrateStatus, CtxIndex, DocStatus, EdgeKind, EmbedError, EmbeddingStore, FileSnapshot,
    HybridWeights, NodeKind, OutputFormat, QueryOpts, RepoSnapshot, SCHEMA_VERSION, SearchError, area_report,
    area_to_value, cosine_similarity, doc_outline, domain_map_to_value, estimate_tokens, extract_trigrams,
    outline_to_value, render_value, search, search_result_to_value, tokenize,
};

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn fixture_index(alloc: Alloc<'static>) -> CtxIndex<'static> {
    let crates = [
        CrateSnapshot {
            dir: "crates/blaze-ty",
            cargo_toml: br#"
[package]
name = "blaze-ty"
description = "Type system, refinement predicates, and terminal persistence types for blaze."

[dependencies]
blaze-data = { workspace = true }
blaze-own = { workspace = true }
"#,
            entry_path: Some("crates/blaze-ty/src/lib.rs"),
            entry_bytes: Some(
                br#"//! Core type representation (docs/v2/plan/design/refined-values.md).
pub struct RefinedType { pub id: u32 }
pub enum PredicateKind { Bounds, NonNull }
pub fn check_refinement_predicate() {}
"#,
            ),
        },
        CrateSnapshot {
            dir: "crates/blaze-data",
            cargo_toml: br#"
[package]
name = "blaze-data"
description = "Format-neutral value and deterministic serialization across CBOR, JSON, TOML, and TAB."

[dependencies]
blaze-own = { workspace = true }
"#,
            entry_path: Some("crates/blaze-data/src/lib.rs"),
            entry_bytes: Some(
                br#"//! One format-neutral value for serialization.
pub enum Value { Null }
pub trait ToValue {}
pub trait FromValue {}
"#,
            ),
        },
        CrateSnapshot {
            dir: "crates/blaze-hvbox",
            cargo_toml: br#"
[package]
name = "blaze-hvbox"
description = "Experimental Hypervisor.framework sandbox for macOS build steps."

[dependencies]
blaze-own = { workspace = true }
"#,
            entry_path: Some("crates/blaze-hvbox/src/lib.rs"),
            entry_bytes: Some(b"//! Experimental macOS hypervisor sandbox.\npub struct VmBox;\n"),
        },
        CrateSnapshot {
            dir: "crates/blaze-parked-old",
            cargo_toml: br#"
[package]
name = "blaze-parked-old"
description = "Old parked legacy crate kept out of workspace."

[dependencies]
"#,
            entry_path: Some("crates/blaze-parked-old/src/lib.rs"),
            entry_bytes: Some(b"//! Old parked crate.\npub fn legacy() {}\n"),
        },
    ];

    let docs = [
        FileSnapshot {
            path: "docs/v2/plan/design/refined-values.md",
            bytes: br#"---
status: plan
summary: "Refinement types, terminal persistence proofs, and deterministic serialization of refined values."
date: 2026-11-25
---
# Refined Values

Refinement predicates live in `crates/blaze-ty` and serialize cleanly with `crates/blaze-data`.

## Terminal Persistence

Persisted refinement proofs survive process restarts via deterministic CBOR records.

```rust
// # This comment inside a code fence is NOT a markdown heading!
fn demo() {}
```

## Serialization Layout

Refined types encode into compact columnar tables. See [bottom.md](bottom.md).
"#,
        },
        FileSnapshot {
            path: "docs/v2/plan/design/bottom.md",
            bytes: br#"---
status: current
summary: "The bottom item layer: keyed records, no in-memory dependency graph, sans-IO cores."
---
# The Bottom Layer

Everything kept is a record keyed by BLAKE3.
"#,
        },
        FileSnapshot {
            path: concat!("docs/", "v2/plan/design/old-types.md"),
            bytes: br#"---
status: superseded
summary: "Old superseded type refinement notes replaced by refined-values.md."
date: 2026-11-10
superseded_by: refined-values.md
---
# Old Type Refinement

Superseded refinement serialization notes.
"#,
        },
    ];

    let howtos = [FileSnapshot {
        path: "agents/howto/H-ty-check.json",
        bytes: br#"{
  "id": "H-ty-check",
  "date": "2026-11-22",
  "task": "Run targeted blaze-ty refinement tests fast",
  "do": "Use blaze test crates/blaze-ty to check refinement predicates.",
  "why": "Avoids cold cargo rebuilds.",
  "state": "current"
}"#,
    }];

    let annoyances = [FileSnapshot {
        path: "agents/annoyances/A-ty-serial.json",
        bytes: br#"{
  "id": "A-ty-serial",
  "date": "2026-11-24",
  "area": "crates/blaze-ty refinement serialization",
  "annoyance": "Forgetting deterministic key ordering in blaze-ty broke CBOR round-trip.",
  "workaround": "Always route through blaze-data and blaze-cbor.",
  "state": "open"
}"#,
    }];

    let excluded = ["crates/blaze-hvbox", "crates/blaze-parked-old"];
    let snap = RepoSnapshot {
        excluded_paths: &excluded,
        crates: &crates,
        docs: &docs,
        howtos: &howtos,
        annoyances: &annoyances,
    };

    let (index, stats) = CtxIndex::build_incremental(None, &snap, alloc);
    assert_eq!(stats.total_crates, 4);
    assert_eq!(stats.total_docs, 3);
    assert_eq!(stats.total_notes, 2);
    assert_eq!(stats.reindexed, 9);
    assert_eq!(stats.reused, 0);
    index
}

#[test]
fn tokenizer_splits_identifiers_camel_snake_kebab_and_paths() {
    let a = heap();
    let toks = tokenize("HTTPServer::parse_refinement_predicates crates/blaze-ty/src/lib.rs FooBar2Baz", a);
    let as_strs: std::vec::Vec<&str> = toks.as_slice().iter().map(AString::as_str).collect();
    assert_eq!(
        as_strs,
        std::vec![
            "http",
            "server",
            "parse",
            "refinement",
            "predicates",
            "crates",
            "blaze",
            "ty",
            "src",
            "lib",
            "rs",
            "foo",
            "bar2",
            "baz"
        ]
    );

    let tris = extract_trigrams(toks.as_slice(), a);
    assert!(tris.contains(b"ref"));
    assert!(tris.contains(b"ser"));
    assert!(tris.contains(b"bla"));
}

#[test]
fn markdown_outline_skips_code_fences_and_computes_exact_line_ranges() {
    let a = heap();
    let index = fixture_index(a);
    let outline = doc_outline(&index, "refined-values.md", a).expect("outline should find refined-values.md");
    assert_eq!(outline.status.as_str(), "plan");
    assert_eq!(outline.headings.len(), 3);
    assert_eq!(outline.headings[0].heading.as_str(), "Refined Values");
    assert_eq!(outline.headings[0].level, 1);
    assert_eq!(outline.headings[1].heading.as_str(), "Terminal Persistence");
    assert_eq!(outline.headings[1].level, 2);
    assert_eq!(outline.headings[2].heading.as_str(), "Serialization Layout");
    assert_eq!(outline.headings[2].level, 2);
    // Ensure code-fence comment `# This comment...` was NOT captured as a heading.
    assert!(outline.headings.as_slice().iter().all(|h| !h.heading.as_str().contains("comment")));
    // Verify `ptr` format `path#Lstart-Lend`.
    assert!(outline.headings[1].ptr.as_str().starts_with("docs/v2/plan/design/refined-values.md#L"));

    // Verify crate statuses were classified accurately.
    let ty = index.crates.as_slice().iter().find(|c| c.name.as_str() == "blaze-ty").unwrap();
    let hv = index.crates.as_slice().iter().find(|c| c.name.as_str() == "blaze-hvbox").unwrap();
    let old = index.crates.as_slice().iter().find(|c| c.name.as_str() == "blaze-parked-old").unwrap();
    assert_eq!(ty.status, CrateStatus::Live);
    assert_eq!(hv.status, CrateStatus::Experimental);
    assert_eq!(old.status, CrateStatus::Parked);

    let sup = index.docs.as_slice().iter().find(|d| d.path.as_str().ends_with("old-types.md")).unwrap();
    assert_eq!(sup.status, DocStatus::Superseded);
}

#[test]
fn hybrid_bm25_trigram_and_1hop_graph_expansion() {
    let a = heap();
    let index = fixture_index(a);

    // Confirm typed graph edges exist between crate, design doc, and annoyance.
    assert!(index.edges.as_slice().iter().any(|e| {
        e.from.as_str() == "crate:blaze-ty"
            && e.to.as_str() == "doc:docs/v2/plan/design/refined-values.md"
            && e.kind == EdgeKind::DesignedBy
    }));
    assert!(index.edges.as_slice().iter().any(|e| {
        e.from.as_str() == "note:annoyance:A-ty-serial"
            && e.to.as_str() == "crate:blaze-ty"
            && e.kind == EdgeKind::AnnoyancesFor
    }));

    // Query with morphological variant ("persisted" vs "persistence") should rank refined-values.md high,
    // ahead of superseded old-types.md, and pinpoint exact heading slices.
    let opts = QueryOpts::new("terminal persisted refinement serialization", 2000);
    let res = search(&index, &opts, a).expect("search should succeed");
    assert!(!res.hits.is_empty());
    let top_ids: std::vec::Vec<&str> = res.hits.as_slice().iter().map(|h| h.id.as_str()).collect();
    assert!(top_ids.contains(&"doc:docs/v2/plan/design/refined-values.md"));
    assert!(top_ids.contains(&"crate:blaze-ty"));

    // Current/plan refined-values.md must outrank superseded old-types.md.
    let pos_plan = top_ids
        .iter()
        .position(|&id| id == "doc:docs/v2/plan/design/refined-values.md")
        .unwrap();
    if let Some(pos_sup) = top_ids.iter().position(|&id| id == concat!("doc:docs/", "v2/plan/design/old-types.md")) {
        assert!(pos_plan < pos_sup, "plan doc should outrank superseded doc");
    }

    // Query specifically targeting a term only in `bottom.md` ("keyed") should 1-hop expand `refined-values.md`
    // via `referenced_by(doc:docs/v2/plan/design/bottom.md)`.
    let hop_opts = QueryOpts::new("BLAKE3 keyed", 2000);
    let hop_res = search(&index, &hop_opts, a).expect("1-hop search should succeed");
    assert!(hop_res.hits.as_slice().iter().any(|h| {
        h.id.as_str() == "doc:docs/v2/plan/design/refined-values.md" && h.via.is_some()
    }));
}

#[test]
fn dense_embedding_store_cosine_fusion_and_strict_errors() {
    let a = heap();
    let mut index = fixture_index(a);

    // Passing a dense vector when store is empty MUST fail explicitly (no silent fallback).
    let mut opts = QueryOpts::new("refinement", 2000);
    let qv = [1.0f32, 0.0, 0.0];
    opts.dense = Some(&qv);
    assert_eq!(search(&index, &opts, a).err(), Some(SearchError::NoEmbeddingsLoaded));

    // Populate embeddings with dim = 3.
    let mut store = EmbeddingStore::new_in(a);
    store.insert("crate:blaze-hvbox", &[0.99, 0.05, 0.0]).unwrap();
    store.insert("crate:blaze-ty", &[0.1, 0.99, 0.0]).unwrap();
    store.insert("doc:docs/v2/plan/design/refined-values.md", &[0.0, 0.0, 1.0]).unwrap();
    // Dimension mismatch on insert is refused.
    assert_eq!(
        store.insert("crate:blaze-data", &[1.0, 2.0]),
        Err(EmbedError::DimensionMismatch { expected: 3, found: 2 })
    );
    // Exact cosine check.
    let cos = cosine_similarity(&[1.0, 0.0, 0.0], &[1.0, 0.0, 0.0]).unwrap();
    assert!((cos - 1.0).abs() < 1e-12);
    let orth = cosine_similarity(&[1.0, 0.0, 0.0], &[0.0, 1.0, 0.0]).unwrap();
    assert!(orth.abs() < 1e-12);

    index.embeddings = store;

    // Dimension mismatch on query is refused with typed error.
    let bad_q = [1.0f32, 0.0];
    opts.dense = Some(&bad_q);
    assert_eq!(
        search(&index, &opts, a).err(),
        Some(SearchError::Embed(EmbedError::DimensionMismatch { expected: 3, found: 2 }))
    );

    // Valid dense query strongly aligned with `crate:blaze-hvbox` surfaces it via dense/hybrid fusion.
    let hv_q = [1.0f32, 0.0, 0.0];
    let mut dense_opts = QueryOpts::new("sandbox", 2000);
    dense_opts.dense = Some(&hv_q);
    dense_opts.weights = Some(HybridWeights { bm25: 0.2, trigram: 0.1, dense: 0.7 });
    let res = search(&index, &dense_opts, a).expect("hybrid dense search should succeed");
    assert_eq!(res.hits[0].id.as_str(), "crate:blaze-hvbox");
    assert_eq!(res.hits[0].kind, NodeKind::Crate);
}

#[test]
fn token_budget_truncation_and_blaze_tab_round_trip() {
    let a = heap();
    let index = fixture_index(a);

    // Small token budget forces deterministic truncation and reports omitted count.
    let tight = QueryOpts::new("refinement serialization persistence blaze", 160);
    let res = search(&index, &tight, a).expect("tight budget search should succeed");
    assert!(res.used_tokens <= res.budget_tokens, "used {} > budget {}", res.used_tokens, res.budget_tokens);
    assert!(res.omitted > 0 || res.hits.len() <= res.total_candidates);

    // Render search, map, outline, and area to `blaze-tab` and verify lossless parse-back via `blaze_tab::read`!
    let search_val = search_result_to_value(&res, a);
    let search_tab = render_value(&search_val, OutputFormat::Tab, a).unwrap();
    assert!(search_tab.as_str().contains("[[rank kind status score"));
    let parsed_search = blaze_tab::read(search_tab.as_bytes(), blaze_tab::Limits::SHIPPED, a).unwrap();
    assert_eq!(parsed_search, search_val);
    assert!(estimate_tokens(search_tab.as_str()) <= tight.budget_tokens);

    let map_val = domain_map_to_value(
        index.domains.as_slice(),
        index.crates.len(),
        index.docs.len(),
        index.notes.len(),
        index.edges.len(),
        a,
    );
    let map_tab = render_value(&map_val, OutputFormat::Tab, a).unwrap();
    let map_json = render_value(&map_val, OutputFormat::Json, a).unwrap();
    // `blaze-tab` must be strictly more compact than JSON!
    assert!(
        map_tab.len() < map_json.len(),
        "expected blaze-tab ({} B) < json ({} B)",
        map_tab.len(),
        map_json.len()
    );
    let parsed_map = blaze_tab::read(map_tab.as_bytes(), blaze_tab::Limits::SHIPPED, a).unwrap();
    assert_eq!(parsed_map, map_val);

    let outline = doc_outline(&index, "docs/v2/plan/design/refined-values.md", a).unwrap();
    let outline_val = outline_to_value(&outline, a);
    let outline_tab = render_value(&outline_val, OutputFormat::Tab, a).unwrap();
    assert_eq!(blaze_tab::read(outline_tab.as_bytes(), blaze_tab::Limits::SHIPPED, a).unwrap(), outline_val);

    let area = area_report(&index, "blaze-ty", a);
    assert!(area.found_crate);
    assert!(!area.design_docs.is_empty());
    assert!(!area.notes.is_empty());
    let area_val = area_to_value(&area, a);
    let area_tab = render_value(&area_val, OutputFormat::Tab, a).unwrap();
    assert_eq!(blaze_tab::read(area_tab.as_bytes(), blaze_tab::Limits::SHIPPED, a).unwrap(), area_val);
}

#[test]
fn incremental_blake3_reuse_and_cbor_round_trip() {
    let a = heap();
    let mut index = fixture_index(a);
    index.embeddings.insert("crate:blaze-ty", &[0.25, 0.75]).unwrap();

    // Deterministic CBOR encode -> decode -> re-encode must be byte-for-byte identical!
    let cbor1 = index.to_cbor();
    let loaded = CtxIndex::from_cbor(cbor1.as_slice(), a).expect("CBOR load should succeed");
    assert_eq!(loaded.schema_version, SCHEMA_VERSION);
    assert_eq!(loaded.crates.len(), index.crates.len());
    assert_eq!(loaded.docs.len(), index.docs.len());
    assert_eq!(loaded.notes.len(), index.notes.len());
    assert_eq!(loaded.edges.len(), index.edges.len());
    assert_eq!(loaded.embeddings.get("crate:blaze-ty"), Some(&[0.25f32, 0.75][..]));
    let cbor2 = loaded.to_cbor();
    assert_eq!(cbor1.as_slice(), cbor2.as_slice(), "CBOR encoding must be bit-for-bit deterministic");

    // Incremental update where 1 doc changes and 1 doc is removed should reuse all unchanged nodes.
    let updated_docs = [
        FileSnapshot {
            path: "docs/v2/plan/design/refined-values.md",
            bytes: b"---\nstatus: current\nsummary: \"Updated refined values spec.\"\n---\n# Refined Values\nUpdated.\n",
        },
        FileSnapshot {
            path: "docs/v2/plan/design/bottom.md",
            bytes: br#"---
status: current
summary: "The bottom item layer: keyed records, no in-memory dependency graph, sans-IO cores."
---
# The Bottom Layer

Everything kept is a record keyed by BLAKE3.
"#,
        },
    ];
    let snap2 = RepoSnapshot {
        excluded_paths: &[],
        crates: &[],
        docs: &updated_docs,
        howtos: &[],
        annoyances: &[],
    };
    let (index2, stats2) = CtxIndex::build_incremental(Some(&loaded), &snap2, a);
    assert_eq!(stats2.reused, 1, "unchanged bottom.md should be reused via BLAKE3 hash");
    assert_eq!(stats2.reindexed, 1, "changed refined-values.md should be reindexed");
    assert_eq!(stats2.removed, 7, "4 crates + 1 doc + 2 notes removed");
    assert_eq!(index2.docs.len(), 2);

    // Schema version mismatch is refused with typed CacheError::SchemaVersionMismatch.
    let mut bad_map = blaze_data::Map::new_in(a);
    bad_map.insert(
        AString::from_str_in("schema_version", a),
        blaze_data::Value::Int(blaze_data::Int::from_u64(999)),
    );
    let bad_cbor = blaze_cbor::to_vec_in(&blaze_data::Value::Map(bad_map), a);
    match CtxIndex::from_cbor(bad_cbor.as_slice(), a) {
        Err(CacheError::SchemaVersionMismatch { found: 999, expected: SCHEMA_VERSION }) => {}
        other => panic!("expected SchemaVersionMismatch, got {:?}", other.err()),
    }
}

#[test]
fn domain_classifier_and_pure_core_math_accuracy() {
    assert_eq!(crate::classify_domain("blaze-c"), "backend");
    assert_eq!(crate::classify_domain("blaze-core"), "runtime-and-exec");
    assert_eq!(crate::classify_domain("blaze-cbor"), "data-and-formats");
    assert_eq!(crate::classify_domain("blaze-ctx"), "data-and-formats");
    assert_eq!(crate::classify_domain("blaze-columns"), "std-and-memory");
    assert_eq!(crate::classify_domain("blaze-build-script"), "storage-and-cache");
    assert_eq!(crate::classify_domain("blaze-ty"), "frontend");
    assert_eq!(crate::classify_domain("blaze-vt"), "ui-cad-and-site");

    for x in [0.001f64, 0.25, 0.5, 1.0, 2.0, 10.0, 1234.567, 1e9] {
        let got_sqrt = crate::math::sqrt_f64(x);
        let ref_sqrt = x.sqrt();
        assert!(
            ((got_sqrt - ref_sqrt) / ref_sqrt).abs() < 1e-13,
            "sqrt({x}): got {got_sqrt}, ref {ref_sqrt}"
        );
        let got_ln = crate::math::ln_f64(x);
        let ref_ln = x.ln();
        assert!(
            (got_ln - ref_ln).abs() < 1e-12,
            "ln({x}): got {got_ln}, ref {ref_ln}"
        );
    }
}
