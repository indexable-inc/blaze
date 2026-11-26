//! Tests for explicit strict vs. lossy decoding, evolving schema round-tripping, finite version
//! dispatch, per-item fault isolation, and `blaze-sim` property tests with planted-bug controls.

use core::fmt;
use std::{
    collections::BTreeMap,
    string::{String as StdString, ToString},
    vec::Vec as StdVec,
};

use blaze_own::{Alloc, String, planted::Planted};
use blaze_sim::{Broken, Config, NoPlant, Tape, controls};

use crate::{
    Cursor, Decoded, FromValue, FromValueLossy, Int, ItemOutcome, Kind, LossyPlant, Map, Shape,
    ShapeError, ToValue, Value, decode_list_isolated, decode_lossy, decode_strict,
    decode_versioned_str, merge_unknown_into, merge_unknown_into_planted,
};

type Heap = Alloc<'static>;

fn heap() -> Heap {
    blaze_own::testing::heap().alloc()
}

fn text(s: &str) -> String<Heap> {
    String::from_str_in(s, heap())
}

// ---------------------------------------------------------------------------
// Evolving schema fixtures: V1 <-> V2 with nested `Theme` structs
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct ThemeV1 {
    name: StdString,
    bold: bool,
}

impl FromValue<Heap> for ThemeV1 {
    fn from_value(c: &Cursor<'_, '_, Heap>) -> Result<ThemeV1, ShapeError<Heap>> {
        c.fields(|f| {
            Ok(ThemeV1 {
                name: f.cursor("name")?.text()?.to_string(),
                bold: f.take_opt("bold")?.unwrap_or(false),
            })
        })
    }
}

impl FromValueLossy<Heap> for ThemeV1 {
    fn from_value_lossy(c: &Cursor<'_, '_, Heap>) -> Result<Decoded<ThemeV1, Heap>, ShapeError<Heap>> {
        c.fields_lossy(|f| {
            Ok(ThemeV1 {
                name: f.cursor("name")?.text()?.to_string(),
                bold: f.opt_or_default("bold", || false)?,
            })
        })
    }
}

impl ToValue<Heap> for ThemeV1 {
    fn to_value(&self, alloc: Heap) -> Value<Heap> {
        value!(in alloc; {
            "name": self.name.as_str(),
            "bold": self.bold,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ConfigV1 {
    schema: StdString,
    font_size: u32,
    theme: ThemeV1,
}

impl FromValue<Heap> for ConfigV1 {
    fn from_value(c: &Cursor<'_, '_, Heap>) -> Result<ConfigV1, ShapeError<Heap>> {
        c.fields(|f| {
            let schema = f.cursor("schema")?.text()?.to_string();
            let font_size = f.take_opt("font_size")?.unwrap_or(12);
            let theme = f.take("theme")?;
            Ok(ConfigV1 { schema, font_size, theme })
        })
    }
}

impl FromValueLossy<Heap> for ConfigV1 {
    fn from_value_lossy(c: &Cursor<'_, '_, Heap>) -> Result<Decoded<ConfigV1, Heap>, ShapeError<Heap>> {
        c.fields_lossy(|f| {
            let schema = f.version_str("schema", &["v1", "v2"])?.to_string();
            let font_size = f.opt_or_default("font_size", || 12)?;
            let theme = f.req_lossy("theme")?;
            Ok(ConfigV1 { schema, font_size, theme })
        })
    }
}

impl ToValue<Heap> for ConfigV1 {
    fn to_value(&self, alloc: Heap) -> Value<Heap> {
        value!(in alloc; {
            "schema": self.schema.as_str(),
            "font_size": self.font_size,
            "theme": self.theme,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ShaderV2 {
    bloom: u32,
}

impl FromValue<Heap> for ShaderV2 {
    fn from_value(c: &Cursor<'_, '_, Heap>) -> Result<ShaderV2, ShapeError<Heap>> {
        c.fields(|f| Ok(ShaderV2 { bloom: f.take("bloom")? }))
    }
}

impl FromValueLossy<Heap> for ShaderV2 {
    fn from_value_lossy(c: &Cursor<'_, '_, Heap>) -> Result<Decoded<ShaderV2, Heap>, ShapeError<Heap>> {
        c.fields_lossy(|f| Ok(ShaderV2 { bloom: f.opt_or_default("bloom", || 0)? }))
    }
}

impl ToValue<Heap> for ShaderV2 {
    fn to_value(&self, alloc: Heap) -> Value<Heap> {
        value!(in alloc; { "bloom": self.bloom })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ThemeV2 {
    name: StdString,
    bold: bool,
    accent: StdString,
    shader: ShaderV2,
}

impl FromValue<Heap> for ThemeV2 {
    fn from_value(c: &Cursor<'_, '_, Heap>) -> Result<ThemeV2, ShapeError<Heap>> {
        c.fields(|f| {
            Ok(ThemeV2 {
                name: f.cursor("name")?.text()?.to_string(),
                bold: f.take("bold")?,
                accent: f.cursor("accent")?.text()?.to_string(),
                shader: f.take("shader")?,
            })
        })
    }
}

impl FromValueLossy<Heap> for ThemeV2 {
    fn from_value_lossy(c: &Cursor<'_, '_, Heap>) -> Result<Decoded<ThemeV2, Heap>, ShapeError<Heap>> {
        c.fields_lossy(|f| {
            Ok(ThemeV2 {
                name: f.cursor("name")?.text()?.to_string(),
                bold: f.opt_or_default("bold", || false)?,
                accent: f.opt_or_default("accent", || text("#00ffaa"))?.as_str().to_string(),
                shader: f.opt_lossy_or_default("shader", || ShaderV2 { bloom: 0 })?,
            })
        })
    }
}

impl ToValue<Heap> for ThemeV2 {
    fn to_value(&self, alloc: Heap) -> Value<Heap> {
        value!(in alloc; {
            "name": self.name.as_str(),
            "bold": self.bold,
            "accent": self.accent.as_str(),
            "shader": self.shader,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ConfigV2 {
    schema: StdString,
    font_size: u32,
    telemetry: bool,
    theme: ThemeV2,
}

impl FromValue<Heap> for ConfigV2 {
    fn from_value(c: &Cursor<'_, '_, Heap>) -> Result<ConfigV2, ShapeError<Heap>> {
        c.fields(|f| {
            let schema = f.cursor("schema")?.text()?.to_string();
            let font_size = f.take("font_size")?;
            let telemetry = f.take("telemetry")?;
            let theme = f.take("theme")?;
            Ok(ConfigV2 { schema, font_size, telemetry, theme })
        })
    }
}

impl FromValueLossy<Heap> for ConfigV2 {
    fn from_value_lossy(c: &Cursor<'_, '_, Heap>) -> Result<Decoded<ConfigV2, Heap>, ShapeError<Heap>> {
        c.fields_lossy(|f| {
            let schema = f.version_str("schema", &["v1", "v2"])?.to_string();
            let font_size = f.opt_or_default("font_size", || 12)?;
            let telemetry = f.opt_or_default("telemetry", || false)?;
            let theme = f.req_lossy("theme")?;
            Ok(ConfigV2 { schema, font_size, telemetry, theme })
        })
    }
}

impl ToValue<Heap> for ConfigV2 {
    fn to_value(&self, alloc: Heap) -> Value<Heap> {
        value!(in alloc; {
            "schema": self.schema.as_str(),
            "font_size": self.font_size,
            "telemetry": self.telemetry,
            "theme": self.theme,
        })
    }
}

// ---------------------------------------------------------------------------
// 1. Strict vs. lossy decoding semantics
// ---------------------------------------------------------------------------

#[test]
fn strict_vs_lossy_missing_required_defaulted_and_unknown_fields() {
    // Missing required field ("theme") is refused in BOTH strict and lossy.
    let missing_req = value!(in heap(); { "schema": "v1", "font_size": 14 });
    let root = Cursor::root(&missing_req, heap());
    assert_eq!(decode_strict::<ConfigV1, _>(&root).unwrap_err().shape, Shape::Missing);
    let lossy_err = decode_lossy::<ConfigV1, _>(&root).unwrap_err();
    assert_eq!(lossy_err.shape, Shape::Missing);
    assert_eq!(lossy_err.to_string(), "theme: missing");

    // Near-miss typo on missing required field still gives the helpful hint in lossy mode.
    let typo_req = value!(in heap(); { "schema": "v1", "theem": { "name": "dark" } });
    let err = decode_lossy::<ConfigV1, _>(&Cursor::root(&typo_req, heap())).unwrap_err();
    assert_eq!(err.to_string(), r#"theme: missing (found unknown field "theem")"#);

    // Wrong kind on a defaulted field is REFUSED (never silently replaced with default!).
    let bad_kind = value!(in heap(); {
        "schema": "v1",
        "font_size": "fourteen",
        "theme": { "name": "dark" },
    });
    let err = decode_lossy::<ConfigV1, _>(&Cursor::root(&bad_kind, heap())).unwrap_err();
    assert_eq!(err.to_string(), "font_size: expected integer, found text");

    // Missing and null defaulted fields are recorded with exact dotted paths.
    let with_defaults = value!(in heap(); {
        "schema": "v1",
        "font_size": null,
        "theme": { "name": "dark" },
    });
    let decoded = decode_lossy::<ConfigV1, _>(&Cursor::root(&with_defaults, heap())).unwrap();
    assert_eq!(
        decoded.value,
        ConfigV1 {
            schema: "v1".into(),
            font_size: 12,
            theme: ThemeV1 { name: "dark".into(), bold: false },
        }
    );
    assert!(!decoded.is_exact());
    assert!(!decoded.has_unknown());
    assert!(decoded.has_defaulted());
    let defaulted: StdVec<&str> = decoded.defaulted.iter().map(String::as_str).collect();
    assert_eq!(defaulted, ["font_size", "theme.bold"]);

    // Unknown extra fields are refused in strict mode, preserved in `unknown` in lossy mode.
    let with_extra = value!(in heap(); {
        "schema": "v1",
        "font_size": 14,
        "future_top": [1, 2, 3],
        "theme": {
            "name": "dark",
            "bold": true,
            "accent": "#ff00aa",
        },
    });
    let cursor = Cursor::root(&with_extra, heap());
    let strict_err = decode_strict::<ConfigV1, _>(&cursor).unwrap_err();
    assert!(matches!(strict_err.shape, Shape::UnknownFields { count: 1 }));
    let decoded = decode_lossy::<ConfigV1, _>(&cursor).unwrap();
    assert!(!decoded.is_exact());
    assert!(decoded.has_unknown());
    assert!(!decoded.has_defaulted());
    assert_eq!(
        decoded.unknown,
        value!(in heap(); {
            "future_top": [1, 2, 3],
            "theme": { "accent": "#ff00aa" },
        })
        .into_map()
        .unwrap()
    );
    let paths = decoded.unknown_paths(heap());
    let path_strs: StdVec<&str> = paths.iter().map(String::as_str).collect();
    assert_eq!(path_strs, ["future_top", "theme.accent"]);

    // Exact document reports `is_exact() == true`.
    let exact = value!(in heap(); {
        "schema": "v1",
        "font_size": 14,
        "theme": { "name": "dark", "bold": true },
    });
    let decoded = decode_lossy::<ConfigV1, _>(&Cursor::root(&exact, heap())).unwrap();
    assert!(decoded.is_exact());
    assert!(!decoded.has_unknown());
    assert!(!decoded.has_defaulted());
    assert!(decoded.unknown_paths(heap()).is_empty());

    // `Decoded::map` preserves `unknown` and `defaulted`.
    let mapped = decoded.map(|cfg| cfg.font_size);
    assert_eq!(mapped.value, 14);
    assert!(mapped.is_exact());
}

// ---------------------------------------------------------------------------
// 2. Full evolving-schema lifecycle: V1 -> V2 -> V1 downgrade + edit -> V2 strict
// ---------------------------------------------------------------------------

#[test]
fn evolving_schema_v1_to_v2_downgrade_and_reupgrade_zero_data_loss() {
    // Step 1: Start with a V1 document on disk.
    let v1_disk = value!(in heap(); {
        "schema": "v1",
        "font_size": 14,
        "theme": {
            "name": "nord",
            "bold": true,
        },
    });

    // Step 2: Upgrade V1 -> V2 using `decode_lossy`; V2 explicitly reports every newly defaulted path.
    let upgraded = decode_lossy::<ConfigV2, _>(&Cursor::root(&v1_disk, heap())).unwrap();
    assert!(!upgraded.has_unknown());
    assert!(upgraded.has_defaulted());
    let defaulted: StdVec<&str> = upgraded.defaulted.iter().map(String::as_str).collect();
    assert_eq!(defaulted, ["telemetry", "theme.accent", "theme.shader"]);

    // Step 3: User configures V2-only features and writes the V2 document to disk.
    let v2_config = ConfigV2 {
        schema: "v2".into(),
        font_size: upgraded.value.font_size,
        telemetry: true,
        theme: ThemeV2 {
            name: upgraded.value.theme.name,
            bold: upgraded.value.theme.bold,
            accent: "#88c0d0".into(),
            shader: ShaderV2 { bloom: 7 },
        },
    };
    let v2_disk = v2_config.to_value(heap());

    // Verify V2 reads strictly.
    assert_eq!(decode_strict::<ConfigV2, _>(&Cursor::root(&v2_disk, heap())).unwrap(), v2_config);

    // Step 4: Older V1 binary opens the V2 file via `decode_lossy`!
    let mut downgraded = decode_lossy::<ConfigV1, _>(&Cursor::root(&v2_disk, heap())).unwrap();
    assert!(downgraded.has_unknown());
    assert!(!downgraded.has_defaulted());
    assert_eq!(
        downgraded.unknown,
        value!(in heap(); {
            "telemetry": true,
            "theme": {
                "accent": "#88c0d0",
                "shader": { "bloom": 7 },
            },
        })
        .into_map()
        .unwrap()
    );

    // Step 5: V1 user edits V1-known fields and saves back using `merge_unknown_into`.
    downgraded.value.font_size = 18;
    downgraded.value.theme.name = "solarized".into();
    let mut saved_map = downgraded.value.to_value(heap()).into_map().unwrap();
    merge_unknown_into(&mut saved_map, &downgraded.unknown, heap());
    let saved_disk = Value::Map(saved_map);

    // Step 6: V2 binary re-opens the V1-edited file STRICTLY with `decode_strict`:
    // ZERO data loss across downgrade + edit + re-upgrade, and V1 edits were preserved!
    let reupgraded = decode_strict::<ConfigV2, _>(&Cursor::root(&saved_disk, heap())).unwrap();
    assert_eq!(
        reupgraded,
        ConfigV2 {
            schema: "v2".into(),
            font_size: 18,
            telemetry: true,
            theme: ThemeV2 {
                name: "solarized".into(),
                bold: true,
                accent: "#88c0d0".into(),
                shader: ShaderV2 { bloom: 7 },
            },
        }
    );
}

#[test]
fn merge_unknown_into_never_overwrites_existing_current_fields() {
    let mut current = value!(in heap(); {
        "font_size": 18,
        "theme": {
            "name": "solarized",
            "nested": 42,
        },
    })
    .into_map()
    .unwrap();

    let unknown = value!(in heap(); {
        "font_size": 999,
        "future": true,
        "theme": {
            "name": "stale",
            "nested": { "would_clobber": true },
            "accent": "#ff00aa",
        },
    })
    .into_map()
    .unwrap();

    merge_unknown_into(&mut current, &unknown, heap());

    assert_eq!(
        Value::Map(current),
        value!(in heap(); {
            "font_size": 18,
            "future": true,
            "theme": {
                "name": "solarized",
                "nested": 42,
                "accent": "#ff00aa",
            },
        })
    );
}

// ---------------------------------------------------------------------------
// 3. Finite version dispatch (`decode_versioned_str`)
// ---------------------------------------------------------------------------

#[test]
fn versioned_str_dispatches_strictly_with_closest_match_hints() {
    let ok_doc = value!(in heap(); { "version": "v2", "payload": 1 });
    let root = Cursor::root(&ok_doc, heap());
    let tag: &str = decode_versioned_str(&root, "version", &["v1", "v2"]).unwrap();
    assert_eq!(tag, "v2");

    // Unknown version -> Shape::UnknownVariant with closest-match suggestion.
    let bad_ver = value!(in heap(); { "version": "v3" });
    let err = decode_versioned_str(&Cursor::root(&bad_ver, heap()), "version", &["v1", "v2"]).unwrap_err();
    assert_eq!(err.shape, Shape::UnknownVariant);
    assert_eq!(
        err.to_string(),
        r#"version: unknown variant "v3" (did you mean "v1"?); known: "v1", "v2""#
    );

    // Missing version key with near-miss typo.
    let typo_key = value!(in heap(); { "verson": "v1" });
    let err = decode_versioned_str(&Cursor::root(&typo_key, heap()), "version", &["v1", "v2"]).unwrap_err();
    assert_eq!(err.shape, Shape::Missing);
    assert_eq!(err.to_string(), r#"version: missing (found unknown field "verson")"#);

    // Non-text version value -> Shape::Expected.
    let int_ver = value!(in heap(); { "version": 2 });
    let err = decode_versioned_str(&Cursor::root(&int_ver, heap()), "version", &["v1", "v2"]).unwrap_err();
    assert_eq!(err.shape, Shape::Expected { expected: Kind::Text, found: Kind::Int });
    assert_eq!(err.to_string(), "version: expected text, found integer");
}

// ---------------------------------------------------------------------------
// 4. Per-item fault isolation (`decode_list_isolated`)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct HistoryBlock {
    version: StdString,
    command: StdString,
    exit_code: i32,
}

impl FromValue<Heap> for HistoryBlock {
    fn from_value(c: &Cursor<'_, '_, Heap>) -> Result<HistoryBlock, ShapeError<Heap>> {
        let _ = decode_versioned_str(c, "version", &["v1"])?;
        c.fields(|f| {
            Ok(HistoryBlock {
                version: f.cursor("version")?.text()?.to_string(),
                command: f.cursor("command")?.text()?.to_string(),
                exit_code: f.take("exit_code")?,
            })
        })
    }
}

impl FromValueLossy<Heap> for HistoryBlock {
    fn from_value_lossy(
        c: &Cursor<'_, '_, Heap>,
    ) -> Result<Decoded<HistoryBlock, Heap>, ShapeError<Heap>> {
        c.fields_lossy(|f| {
            let version = f.version_str("version", &["v1"])?.to_string();
            let command = f.cursor("command")?.text()?.to_string();
            let exit_code = f.opt_or_default("exit_code", || 0)?;
            Ok(HistoryBlock { version, command, exit_code })
        })
    }
}

#[test]
fn list_fault_isolation_degrades_corrupt_blocks_without_losing_session() {
    let session = value!(in heap(); {
        "blocks": [
            { "version": "v1", "command": "cargo check", "exit_code": 0 },
            { "version": "v1", "command": "broken", "exit_code": "not-an-int" },
            { "version": "v1", "command": "git status", "exit_code": 0 },
            { "version": "v99", "command": "future block", "exit_code": 0 },
        ]
    });

    let root = Cursor::root(&session, heap());
    let blocks_cursor = root.field("blocks").unwrap();
    let isolated = decode_list_isolated(&blocks_cursor, decode_strict::<HistoryBlock, _>).unwrap();

    assert_eq!(isolated.len(), 4);
    assert_eq!(isolated.ok_count(), 2);
    assert_eq!(isolated.degraded_count(), 2);
    assert!(isolated.has_degraded());
    assert!(!isolated.is_all_ok());

    let healthy: StdVec<&str> = isolated.ok_items().map(|b| b.command.as_str()).collect();
    assert_eq!(healthy, ["cargo check", "git status"]);

    let degraded: StdVec<_> = isolated.degraded_items().collect();
    assert_eq!(degraded.len(), 2);
    assert_eq!(degraded[0].0, 1);
    assert_eq!(degraded[0].1.to_string(), "blocks[1].exit_code: expected integer, found text");
    assert_eq!(
        *degraded[0].2,
        value!(in heap(); { "version": "v1", "command": "broken", "exit_code": "not-an-int" })
    );

    assert_eq!(degraded[1].0, 3);
    assert_eq!(degraded[1].1.shape, Shape::UnknownVariant);
    assert_eq!(
        degraded[1].1.to_string(),
        r#"blocks[3].version: unknown variant "v99" (did you mean "v1"?); known: "v1""#
    );
    assert_eq!(
        *degraded[1].2,
        value!(in heap(); { "version": "v99", "command": "future block", "exit_code": 0 })
    );

    // Composes with `decode_lossy` directly too (preserving per-item `Decoded` with full `[i]` paths).
    let lossy_session = value!(in heap(); {
        "blocks": [
            { "version": "v1", "command": "ls", "future_meta": 42 },
            "not-even-a-map",
        ]
    });
    let lossy_root = Cursor::root(&lossy_session, heap());
    let blocks_cursor = lossy_root.field("blocks").unwrap();
    let lossy_list = decode_list_isolated(&blocks_cursor, decode_lossy::<HistoryBlock, _>).unwrap();
    assert_eq!(lossy_list.ok_count(), 1);
    assert_eq!(lossy_list.degraded_count(), 1);
    match &lossy_list.items[0] {
        ItemOutcome::Ok(decoded) => {
            assert_eq!(decoded.value.command, "ls");
            assert_eq!(decoded.value.exit_code, 0);
            let defaulted: StdVec<&str> = decoded.defaulted.iter().map(String::as_str).collect();
            assert_eq!(defaulted, ["blocks[0].exit_code"]);
            assert_eq!(decoded.unknown.get("future_meta"), Some(&Value::Int(Int::from_u64(42))));
        }
        ItemOutcome::Degraded { .. } => panic!("block 0 should decode lossily"),
    }
    match &lossy_list.items[1] {
        ItemOutcome::Degraded { index, error, raw } => {
            assert_eq!(*index, 1);
            assert_eq!(error.to_string(), "blocks[1]: expected map, found text");
            assert_eq!(*raw, value!(in heap(); "not-even-a-map"));
        }
        ItemOutcome::Ok(_) => panic!("block 1 should be degraded"),
    }

    // Non-list container is still refused at the container level.
    let not_a_list = value!(in heap(); { "blocks": 123 });
    let err = decode_list_isolated(
        &Cursor::root(&not_a_list, heap()).field("blocks").unwrap(),
        decode_strict::<HistoryBlock, _>,
    )
    .unwrap_err();
    assert_eq!(err.to_string(), "blocks: expected list, found integer");
}

// ---------------------------------------------------------------------------
// 5. `blaze-sim` property test + planted-bug controls
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct SimNested {
    req_n: u64,
    opt_a: u64,
    opt_b: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct SimDoc {
    req_id: u64,
    opt_x: u64,
    opt_y: u64,
    nested: SimNested,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum LossyBroken {
    /// `Decoded::unknown` did not match the model's exact unknown sub-map.
    UnknownMismatch { got_keys: StdVec<StdString>, want_keys: StdVec<StdString> },
    /// `Decoded::defaulted` did not match the model's exact list of defaulted dotted paths.
    DefaultedMismatch { got: StdVec<StdString>, want: StdVec<StdString> },
    /// `is_exact` / `has_unknown` / `has_defaulted` disagreed with `unknown` and `defaulted`.
    FlagsMismatch,
    /// Round-tripping through `merge_unknown_into_planted` lost an unknown field.
    MergeLostUnknown { key: StdString },
    /// `merge_unknown_into_planted` overwrote a current field.
    MergeOverwroteCurrent { key: StdString },
}

impl fmt::Display for LossyBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LossyBroken::UnknownMismatch { got_keys, want_keys } => {
                write!(f, "unknown mismatch: got {got_keys:?}, want {want_keys:?}")
            }
            LossyBroken::DefaultedMismatch { got, want } => {
                write!(f, "defaulted mismatch: got {got:?}, want {want:?}")
            }
            LossyBroken::FlagsMismatch => write!(f, "Decoded helper flags disagreed with contents"),
            LossyBroken::MergeLostUnknown { key } => {
                write!(f, "merge_unknown_into lost unknown field {key:?}")
            }
            LossyBroken::MergeOverwroteCurrent { key } => {
                write!(f, "merge_unknown_into overwrote current field {key:?}")
            }
        }
    }
}

fn lossy_agrees(tape: &Tape, plant: impl Planted<LossyPlant>) -> Result<(), Broken<LossyBroken>> {
    // Generate a random subset of present/missing/null optional fields and extra unknown fields
    // at both top-level and nested levels.
    let top_opt_x = tape.range(0..3, "top_opt_x"); // 0 = missing, 1 = null, 2 = present
    let top_opt_y = tape.range(0..3, "top_opt_y");
    let nested_opt_a = tape.range(0..3, "nested_opt_a");
    let nested_opt_b = tape.range(0..3, "nested_opt_b");

    let top_extra_pool = ["extra_1", "extra_2", "a.dotted", "sub_map"];
    let nested_extra_pool = ["n_extra_1", "n_extra_2", "k.dot"];

    let mut top_extras: BTreeMap<&str, u64> = BTreeMap::new();
    for &k in &top_extra_pool {
        if tape.coin("include_top_extra") {
            top_extras.insert(k, tape.range(1..100, "top_extra_val"));
        }
    }
    let mut nested_extras: BTreeMap<&str, u64> = BTreeMap::new();
    for &k in &nested_extra_pool {
        if tape.coin("include_nested_extra") {
            nested_extras.insert(k, tape.range(1..100, "nested_extra_val"));
        }
    }

    // Build nested input map.
    let mut nested_map = Map::new_in(heap());
    nested_map.insert(text("req_n"), Value::Int(Int::from_u64(7)));
    let mut want_defaulted: StdVec<StdString> = StdVec::new();
    if top_opt_x != 2 {
        want_defaulted.push("opt_x".to_string());
    }
    if top_opt_y != 2 {
        want_defaulted.push("opt_y".to_string());
    }
    if nested_opt_a == 1 {
        nested_map.insert(text("opt_a"), Value::Null);
    } else if nested_opt_a == 2 {
        nested_map.insert(text("opt_a"), Value::Int(Int::from_u64(11)));
    }
    if nested_opt_a != 2 {
        want_defaulted.push("nested.opt_a".to_string());
    }
    if nested_opt_b == 1 {
        nested_map.insert(text("opt_b"), Value::Null);
    } else if nested_opt_b == 2 {
        nested_map.insert(text("opt_b"), Value::Int(Int::from_u64(22)));
    }
    if nested_opt_b != 2 {
        want_defaulted.push("nested.opt_b".to_string());
    }
    for (&k, &v) in &nested_extras {
        nested_map.insert(text(k), Value::Int(Int::from_u64(v)));
    }

    // Build top-level input map.
    let mut top_map = Map::new_in(heap());
    top_map.insert(text("req_id"), Value::Int(Int::from_u64(1)));
    if top_opt_x == 1 {
        top_map.insert(text("opt_x"), Value::Null);
    } else if top_opt_x == 2 {
        top_map.insert(text("opt_x"), Value::Int(Int::from_u64(10)));
    }
    if top_opt_y == 1 {
        top_map.insert(text("opt_y"), Value::Null);
    } else if top_opt_y == 2 {
        top_map.insert(text("opt_y"), Value::Int(Int::from_u64(20)));
    }
    top_map.insert(text("nested"), Value::Map(nested_map));
    for (&k, &v) in &top_extras {
        let val = if k == "sub_map" {
            value!(in heap(); { "inner": v })
        } else {
            Value::Int(Int::from_u64(v))
        };
        top_map.insert(text(k), val);
    }
    let input = Value::Map(top_map);

    // Decode lossily with the active plant.
    let root = Cursor::root(&input, heap());
    let decoded = root
        .fields_lossy_planted(plant, |f| {
            let req_id = f.req::<u64>("req_id")?;
            let opt_x = f.opt_or_default("opt_x", || 100u64)?;
            let opt_y = f.opt_or_default("opt_y", || 200u64)?;
            let nested_cursor = f.cursor("nested")?;
            let nested_decoded = nested_cursor.fields_lossy_planted(plant, |nf| {
                Ok::<_, ShapeError<Heap>>(SimNested {
                    req_n: nf.req::<u64>("req_n")?,
                    opt_a: nf.opt_or_default("opt_a", || 300u64)?,
                    opt_b: nf.opt_or_default("opt_b", || 400u64)?,
                })
            })?;
            let nested = f.merge_child("nested", nested_decoded);
            Ok::<_, ShapeError<Heap>>(SimDoc { req_id, opt_x, opt_y, nested })
        })
        .expect("generated doc is always well-typed");

    // Oracle 1: Build exact expected `unknown` sub-map.
    let mut want_unknown = Map::new_in(heap());
    if !nested_extras.is_empty() {
        let mut sub = Map::new_in(heap());
        for (&k, &v) in &nested_extras {
            sub.insert(text(k), Value::Int(Int::from_u64(v)));
        }
        want_unknown.insert(text("nested"), Value::Map(sub));
    }
    for (&k, &v) in &top_extras {
        let val = if k == "sub_map" {
            value!(in heap(); { "inner": v })
        } else {
            Value::Int(Int::from_u64(v))
        };
        want_unknown.insert(text(k), val);
    }
    if decoded.unknown != want_unknown {
        let got_keys = decoded.unknown.sorted().map(|(k, _)| k.to_string()).collect();
        let want_keys = want_unknown.sorted().map(|(k, _)| k.to_string()).collect();
        return Err(Broken::Oracle(LossyBroken::UnknownMismatch { got_keys, want_keys }));
    }

    // Oracle 2: Exact dotted paths in `defaulted`.
    let got_defaulted: StdVec<StdString> =
        decoded.defaulted.iter().map(|s| s.as_str().to_string()).collect();
    if got_defaulted != want_defaulted {
        return Err(Broken::Oracle(LossyBroken::DefaultedMismatch {
            got: got_defaulted,
            want: want_defaulted,
        }));
    }

    // Oracle 3: Helper flags.
    let want_has_unknown = !want_unknown.is_empty();
    let want_has_defaulted = !want_defaulted.is_empty();
    if decoded.has_unknown() != want_has_unknown
        || decoded.has_defaulted() != want_has_defaulted
        || decoded.is_exact() != (!want_has_unknown && !want_has_defaulted)
    {
        return Err(Broken::Oracle(LossyBroken::FlagsMismatch));
    }

    // Oracle 4: Downgrade-safe re-serialization + non-overwrite check via `merge_unknown_into_planted`.
    let mut resaved = value!(in heap(); {
        "req_id": decoded.value.req_id + 1000,
        "opt_x": decoded.value.opt_x,
        "opt_y": decoded.value.opt_y,
        "nested": {
            "req_n": decoded.value.nested.req_n + 500,
            "opt_a": decoded.value.nested.opt_a,
            "opt_b": decoded.value.nested.opt_b,
        },
    })
    .into_map()
    .unwrap();

    // Also include a colliding key in a synthetic unknown overlay to exercise non-overwrite.
    let mut overlay = decoded.unknown.copy_in(heap());
    overlay.insert(text("req_id"), Value::Int(Int::from_u64(0)));
    merge_unknown_into_planted(&mut resaved, &overlay, heap(), plant);

    if resaved.get("req_id").and_then(Value::as_u64) != Some(decoded.value.req_id + 1000) {
        return Err(Broken::Oracle(LossyBroken::MergeOverwroteCurrent {
            key: "req_id".to_string(),
        }));
    }
    for (&k, &v) in &top_extras {
        let want_val = if k == "sub_map" {
            value!(in heap(); { "inner": v })
        } else {
            Value::Int(Int::from_u64(v))
        };
        if resaved.get(k) != Some(&want_val) {
            return Err(Broken::Oracle(LossyBroken::MergeLostUnknown { key: k.to_string() }));
        }
    }
    let resaved_nested = resaved.get("nested").and_then(Value::as_map).unwrap();
    for (&k, &v) in &nested_extras {
        if resaved_nested.get(k) != Some(&Value::Int(Int::from_u64(v))) {
            return Err(Broken::Oracle(LossyBroken::MergeLostUnknown {
                key: std::format!("nested.{k}"),
            }));
        }
    }

    Ok(())
}

blaze_sim::plants! {
    /// Bugs planted in lossy decoding and merging, each paired with the oracle invariant that
    /// must catch it.
    enum LossyControl breaks LossyBroken {
        /// Lossy decoding as shipped.
        None,
        /// Dropping unknown fields must break the `UnknownMismatch` invariant.
        DropUnknownFields => LossyBroken::UnknownMismatch { .. },
        /// Dropping a defaulted field path must break the `DefaultedMismatch` invariant.
        DropDefaultedPath => LossyBroken::DefaultedMismatch { .. },
        /// Overwriting a current field during merge must break `MergeOverwroteCurrent`.
        OverwriteOnMerge => LossyBroken::MergeOverwroteCurrent { .. },
    }
}

impl LossyControl {
    fn planted(self) -> LossyPlant {
        match self {
            LossyControl::None => LossyPlant::None,
            LossyControl::DropUnknownFields => LossyPlant::DropUnknownFields,
            LossyControl::DropDefaultedPath => LossyPlant::DropDefaultedPath,
            LossyControl::OverwriteOnMerge => LossyPlant::OverwriteOnMerge,
        }
    }
}

#[test]
fn lossy_decoding_accounts_for_every_extra_and_defaulted_field() {
    blaze_sim::check(Config::cases(500), |tape| lossy_agrees(tape, NoPlant));
}

#[test]
fn every_lossy_plant_is_caught_by_its_invariant() {
    controls(Config::cases(500), |tape, plant: LossyControl| lossy_agrees(tape, plant.planted()));
}

#[test]
fn deep_merge_and_unknown_paths_run_without_recursion() {
    // 64 KiB stack would overflow if `merge_unknown_into` or `unknown_paths` recursed on depth.
    let thread = std::thread::Builder::new()
        .stack_size(64 << 10)
        .spawn(|| {
            let depth = 4_000usize;
            let mut dst_val = value!(in heap(); { "keep": 1 });
            let mut src_val = value!(in heap(); { "extra": 2 });
            for _ in 0..depth {
                let mut d = Map::new_in(heap());
                d.insert(text("n"), dst_val);
                dst_val = Value::Map(d);
                let mut s = Map::new_in(heap());
                s.insert(text("n"), src_val);
                src_val = Value::Map(s);
            }
            let mut dst = dst_val.into_map().unwrap();
            let src = src_val.into_map().unwrap();

            let decoded = Decoded { value: (), unknown: src.copy_in(heap()), defaulted: blaze_own::Vec::new_in(heap()) };
            let paths = decoded.unknown_paths(heap());
            assert_eq!(paths.len(), 1);
            assert!(paths[0].as_str().ends_with(".extra"));

            merge_unknown_into(&mut dst, &src, heap());
            // Walk to the bottom iteratively to confirm both `keep` and `extra` survived.
            let mut cur = &dst;
            for _ in 0..depth {
                cur = cur.get("n").and_then(Value::as_map).unwrap();
            }
            assert_eq!(cur.get("keep").and_then(Value::as_u64), Some(1));
            assert_eq!(cur.get("extra").and_then(Value::as_u64), Some(2));
        })
        .unwrap();
    thread.join().unwrap();
}
