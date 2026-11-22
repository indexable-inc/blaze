//! Structured decision & Q&A engine for `blaze-bstd-convert`.
//!
//! Wherever a conversion has a genuine architectural trade-off (replacing vs wrapping vs keeping a
//! dependency, rewriting custom proc macros with `comptime`/`const` reflection, lifting imperative
//! IO functions into explicit sans-IO state machines vs direct capability threading, splitting
//! mixed-IO structs, choosing allocator/hasher/lock strategies), the converter generates a grouped,
//! deterministic list of [`Question`]s and records answers in a [`Decisions`] value serialized as
//! strict RFC 8259 JSON via `blaze-json`.

use alloc::{
    borrow::ToOwned,
    collections::BTreeMap,
    format,
    string::String,
    vec,
    vec::Vec,
};

use blaze_data::{Map, Value};
use blaze_json::{KeyOrder, Limits, Style, WriteOptions, read_in, to_string_in};
use blaze_own::{Alloc, String as OwnString, Vec as OwnVec};

use crate::{
    analyze::WorkspaceAnalysis,
    catalog::{DepDisposition, lookup_crate},
};

/// Category of a conversion decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QuestionCategory {
    Dependency,
    ProcMacroToComptime,
    CInterop,
    Newtype,
    StateMachineLift,
    ImplicitStateStruct,
    Allocator,
    HashMapPolicy,
    SyncPolicy,
}

impl QuestionCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dependency => "dependency",
            Self::ProcMacroToComptime => "proc-macro-comptime",
            Self::CInterop => "c-interop",
            Self::Newtype => "newtype",
            Self::StateMachineLift => "state-machine-lift",
            Self::ImplicitStateStruct => "implicit-state-struct",
            Self::Allocator => "allocator",
            Self::HashMapPolicy => "hashmap-policy",
            Self::SyncPolicy => "sync-policy",
        }
    }
}

/// One selectable answer option for a [`Question`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    /// Stable machine key written to `decisions.json` (e.g. `"replace_bstd"`, `"keep_external"`).
    pub key: String,
    /// Human-readable label.
    pub label: String,
    /// Concrete impact of choosing this option.
    pub impact: String,
    /// Whether this is the recommended `bstd`-first option.
    pub recommended: bool,
}

/// One structured question surfaced to the user or agent before converting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    pub category: QuestionCategory,
    pub prompt: String,
    pub affected: Vec<String>,
    pub options: Vec<QuestionOption>,
}

impl Question {
    pub fn default_key(&self) -> &str {
        self.options
            .iter()
            .find(|o| o.recommended)
            .or_else(|| self.options.first())
            .map(|o| o.key.as_str())
            .unwrap_or("")
    }
}

/// Recorded answers to conversion questions (deterministic, JSON round-trippable).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decisions {
    /// `question_id -> chosen option key`.
    pub answers: BTreeMap<String, String>,
    /// Optional path to `bstd` (`crates/blaze-std`) inserted into rewritten `Cargo.toml`s.
    pub bstd_path: Option<String>,
}

impl Decisions {
    /// Populate every question with its recommended `bstd`-first option.
    pub fn from_recommended(questions: &[Question]) -> Self {
        let mut answers = BTreeMap::new();
        for q in questions {
            answers.insert(q.id.clone(), q.default_key().to_owned());
        }
        Self { answers, bstd_path: None }
    }

    pub fn get(&self, id: &str) -> Option<&str> {
        self.answers.get(id).map(String::as_str)
    }

    /// Look up the effective strategy for a third-party dependency package.
    pub fn dep_choice(&self, package: &str) -> DepChoice {
        let key = format!("dep:{package}");
        match self.get(&key) {
            Some("replace_bstd") => DepChoice::ReplaceBstd,
            Some("wrap_sans_io_trait") => DepChoice::WrapSansIoTrait,
            Some("keep_external") => DepChoice::KeepExternal,
            _ => match lookup_crate(package).map(|m| m.disposition) {
                Some(DepDisposition::DirectBstd) => DepChoice::ReplaceBstd,
                Some(DepDisposition::KeepExternal) => DepChoice::KeepExternal,
                Some(DepDisposition::NeedsDecision) => DepChoice::ReplaceBstd,
                Some(DepDisposition::Unknown) | None => DepChoice::KeepExternal,
            },
        }
    }

    pub fn sm_lift_mode(&self) -> SmLiftMode {
        match self.get("io:state_machine_mode") {
            Some("lift_all_candidates") => SmLiftMode::LiftAllCandidates,
            Some("thread_capabilities_only") => SmLiftMode::ThreadCapabilitiesOnly,
            _ => SmLiftMode::LiftAllCandidates,
        }
    }

    pub fn alloc_strategy(&self) -> AllocStrategy {
        match self.get("alloc:strategy") {
            Some("explicit_alloc_handle") => AllocStrategy::ExplicitAllocHandle,
            Some("cx_frame_scratch") => AllocStrategy::CxFrameScratch,
            Some("keep_alloc_for_now") => AllocStrategy::KeepAllocForNow,
            _ => AllocStrategy::ExplicitAllocHandle,
        }
    }

    pub fn parameterize_struct_fields(&self) -> bool {
        self.get("alloc:struct_fields") != Some("keep_struct_fields")
    }

    pub fn emit_newtypes_module(&self) -> bool {
        self.get("newtype:promote_primitives") != Some("report_only")
    }

    pub fn comptime_proc_macro_choice(&self, crate_name: &str) -> ComptimeChoice {
        let key = format!("procmacro:{crate_name}");
        match self.get(&key) {
            Some("comptime_reflection") => ComptimeChoice::ComptimeReflection,
            Some("declarative_macro") => ComptimeChoice::DeclarativeMacro,
            Some("keep_wasm_proc_macro") => ComptimeChoice::KeepWasmProcMacro,
            _ => ComptimeChoice::ComptimeReflection,
        }
    }

    pub fn c_interop_choice(&self, crate_name: &str) -> CInteropChoice {
        let key = format!("c_interop:{crate_name}");
        match self.get(&key) {
            Some("blaze_c_import") => CInteropChoice::BlazeCImport,
            Some("keep_build_script") => CInteropChoice::KeepBuildScript,
            _ => CInteropChoice::BlazeCImport,
        }
    }

    /// Serialize `Decisions` to strict deterministic pretty JSON using `blaze-json`.
    pub fn to_json(&self) -> Result<String, crate::ConvertError> {
        let alloc = Alloc::global();
        let mut root = Map::new_in(alloc);
        root.insert(
            OwnString::from_str_in("version", alloc),
            Value::Int(blaze_data::Int::from_u64(1)),
        );
        if let Some(p) = &self.bstd_path {
            root.insert(
                OwnString::from_str_in("bstd_path", alloc),
                Value::Text(OwnString::from_str_in(p, alloc)),
            );
        }
        let mut ans = Map::new_in(alloc);
        for (k, v) in &self.answers {
            ans.insert(
                OwnString::from_str_in(k, alloc),
                Value::Text(OwnString::from_str_in(v, alloc)),
            );
        }
        root.insert(OwnString::from_str_in("answers", alloc), Value::Map(ans));

        let opts = WriteOptions {
            keys: KeyOrder::Sorted,
            style: Style::Pretty { indent: 2 },
            bytes: blaze_json::BytesAs::Refuse,
        };
        let out = to_string_in(&Value::Map(root), opts, alloc)
            .map_err(|e| crate::ConvertError::JsonWrite { detail: format!("{e}") })?;
        Ok(out.as_str().to_owned())
    }

    /// Deserialize `Decisions` from strict RFC 8259 JSON using `blaze-json`.
    pub fn from_json(text: &str) -> Result<Self, crate::ConvertError> {
        let alloc = Alloc::global();
        let val = read_in(text.as_bytes(), Limits::SHIPPED, alloc)
            .map_err(|e| crate::ConvertError::JsonParse { detail: format!("{e}") })?;
        let Value::Map(ref root) = val else {
            return Err(crate::ConvertError::InvalidDecisionsField {
                field: "<root>".to_owned(),
                expected: "a JSON object",
            });
        };
        let bstd_path = match root.get("bstd_path") {
            Some(Value::Text(s)) => Some(s.as_str().to_owned()),
            Some(Value::Null) | None => None,
            _ => {
                return Err(crate::ConvertError::InvalidDecisionsField {
                    field: "bstd_path".to_owned(),
                    expected: "a string or null",
                });
            }
        };
        let mut answers = BTreeMap::new();
        if let Some(Value::Map(map)) = root.get("answers") {
            for (k, v) in map.iter() {
                if let Value::Text(s) = v {
                    answers.insert(k.to_owned(), s.as_str().to_owned());
                } else {
                    return Err(crate::ConvertError::InvalidDecisionsField {
                        field: format!("answers.{k}"),
                        expected: "a string",
                    });
                }
            }
        }
        Ok(Self { answers, bstd_path })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepChoice {
    ReplaceBstd,
    WrapSansIoTrait,
    KeepExternal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmLiftMode {
    LiftAllCandidates,
    ThreadCapabilitiesOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocStrategy {
    ExplicitAllocHandle,
    CxFrameScratch,
    KeepAllocForNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComptimeChoice {
    ComptimeReflection,
    DeclarativeMacro,
    KeepWasmProcMacro,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CInteropChoice {
    BlazeCImport,
    KeepBuildScript,
}

/// Build the prioritized, grouped list of [`Question`]s for a workspace analysis.
pub fn build_questions(analysis: &WorkspaceAnalysis) -> Vec<Question> {
    let mut questions = Vec::new();

    // 1. Global allocator strategy (whenever alloc sites exist).
    let total_alloc: usize = analysis.crates.iter().map(|c| c.alloc_sites).sum();
    if total_alloc > 0 {
        questions.push(Question {
            id: "alloc:strategy".to_owned(),
            category: QuestionCategory::Allocator,
            prompt: format!(
                "Found {total_alloc} implicit-heap allocation sites (`Vec`, `String`, `Box`, `format!`, `vec!`). How should `bstd` memory allocation be threaded?"
            ),
            affected: vec![format!("{total_alloc} sites across {} crates", analysis.crates.len())],
            options: vec![
                QuestionOption {
                    key: "explicit_alloc_handle".to_owned(),
                    label: "Thread `alloc: bstd::mem::Alloc<'_>` (Recommended)".to_owned(),
                    impact: "One-word Copy allocator handle; zero generics added, works with `cx.frame()` or `Heap`.".to_owned(),
                    recommended: true,
                },
                QuestionOption {
                    key: "cx_frame_scratch".to_owned(),
                    label: "Use `cx.frame()` on task-scoped functions".to_owned(),
                    impact: "Scratch allocations freed automatically when each task ends; requires `cx: &bstd::Cx`.".to_owned(),
                    recommended: false,
                },
                QuestionOption {
                    key: "keep_alloc_for_now".to_owned(),
                    label: "Keep `alloc::*` temporarily and convert IO/deps first".to_owned(),
                    impact: "Fastest first step to 0 third-party deps + sans-IO state machines; migrate allocators in pass 2.".to_owned(),
                    recommended: false,
                },
            ],
        });
        questions.push(Question {
            id: "alloc:struct_fields".to_owned(),
            category: QuestionCategory::Allocator,
            prompt: "Parameterize plain owned-heap structs (`Vec<T>`, `String`, `Box<T>`, `HashMap`, `BTreeMap` fields) with `<'a>` and explicit `bstd::mem::Alloc<'a>` fields + `impl<'a>` blocks?".to_owned(),
            affected: vec![format!("non-serde, non-repr(C) domain structs across {} crates", analysis.crates.len())],
            options: vec![
                QuestionOption {
                    key: "parameterize_struct_fields".to_owned(),
                    label: "Parameterize heap structs with `<'a>` and `bstd::mem::Alloc<'a>` (Recommended)".to_owned(),
                    impact: "Rewrites `struct Foo { items: Vec<String> }` -> `struct Foo<'a> { items: bstd::vec::Vec<bstd::mem::String<bstd::mem::Alloc<'a>>, bstd::mem::Alloc<'a>> }` and updates matching `impl<'a> Foo<'a>` blocks.".to_owned(),
                    recommended: true,
                },
                QuestionOption {
                    key: "keep_struct_fields".to_owned(),
                    label: "Keep struct field types untouched (only thread function params & return types)".to_owned(),
                    impact: "Leaves struct definitions unchanged for incremental migration.".to_owned(),
                    recommended: false,
                },
            ],
        });
    }

    // 2. Imperative IO -> Explicit Sans-IO State Machines strategy.
    if analysis.total_sm_fn_candidates > 0 || analysis.total_effect_sites > 0 {
        let sample: Vec<String> = analysis
            .crates
            .iter()
            .flat_map(|c| c.functions.iter().filter(|f| f.is_sm_candidate()).map(move |f| format!("{}::{} ({}:{}, {} IO sites)", c.name, f.qualified_name, f.file, f.line, f.direct_effects.len())))
            .take(12)
            .collect();
        questions.push(Question {
            id: "io:state_machine_mode".to_owned(),
            category: QuestionCategory::StateMachineLift,
            prompt: format!(
                "Found {} multi-step IO / async functions ({} total IO sites). Compile them into explicit `#![blaze::sans_io]` state machines + thin `bstd` drivers?",
                analysis.total_sm_fn_candidates, analysis.total_effect_sites
            ),
            affected: sample,
            options: vec![
                QuestionOption {
                    key: "lift_all_candidates".to_owned(),
                    label: "Compile into explicit sans-IO state machines + thin `bstd` drivers (Recommended)".to_owned(),
                    impact: "Generates `Effect`/`Event`/`Step`/`State` enums with exact live-variable analysis, pure `step()`, thin `bstd` driver, and deterministic sim test.".to_owned(),
                    recommended: true,
                },
                QuestionOption {
                    key: "thread_capabilities_only".to_owned(),
                    label: "Thread narrowed `bstd` capabilities directly (`&Dir`, `&Net`, `&Clock`, `&Env`)".to_owned(),
                    impact: "Keeps imperative function shape while removing ambient `std` authority (still simulatable via `bstd::test::Sim`).".to_owned(),
                    recommended: false,
                },
            ],
        });
    }

    // 3. Implicit state-machine / mixed-IO structs.
    if analysis.total_sm_struct_candidates > 0 {
        let sample: Vec<String> = analysis
            .crates
            .iter()
            .flat_map(|c| {
                c.struct_sm_candidates.iter().map(move |s| {
                    format!("{}::{} ({}:{}) — {}", c.name, s.name, s.file, s.line, s.reason())
                })
            })
            .take(10)
            .collect();
        questions.push(Question {
            id: "struct:split_mixed_io".to_owned(),
            category: QuestionCategory::ImplicitStateStruct,
            prompt: format!(
                "Found {} structs mixing live IO handles or implicit bool/Option/phase flags. Refactor them into explicit sans-IO core state enums + thin drivers?",
                analysis.total_sm_struct_candidates
            ),
            affected: sample,
            options: vec![
                QuestionOption {
                    key: "split_into_core_and_driver".to_owned(),
                    label: "Generate `<Struct>Core` sans-IO state enum + `<Struct>Driver` (Recommended)".to_owned(),
                    impact: "Separates sockets/files/clocks from state transitions so protocol/domain logic is 100% `#![blaze::sans_io]`.".to_owned(),
                    recommended: true,
                },
                QuestionOption {
                    key: "report_only".to_owned(),
                    label: "Report only (do not emit refactored struct skeletons)".to_owned(),
                    impact: "Lists every implicit state machine in the report for manual refactoring.".to_owned(),
                    recommended: false,
                },
            ],
        });
    }

    // 4. Custom proc-macro crates -> `comptime` / `const` reflection!
    for krate in &analysis.crates {
        if !krate.is_proc_macro && krate.proc_macro_entries.is_empty() {
            continue;
        }
        let entries_desc: Vec<String> = krate
            .proc_macro_entries
            .iter()
            .map(|e| {
                let label = e.derive_name.as_deref().unwrap_or(&e.fn_name);
                format!(
                    "#[{}({})] at {}:{} -> {}",
                    e.kind.as_str(),
                    label,
                    e.file,
                    e.line,
                    e.pattern.comptime_recommendation()
                )
            })
            .collect();
        questions.push(Question {
            id: format!("procmacro:{}", krate.name),
            category: QuestionCategory::ProcMacroToComptime,
            prompt: format!(
                "Custom proc-macro crate `{}` defines {} entry point(s) and pulls in `syn`/`quote`/`proc-macro2`. Rewrite with `comptime` / `const` reflection (`bstd::comptime` + `core::mem::type_info`)?",
                krate.name,
                krate.proc_macro_entries.len()
            ),
            affected: entries_desc,
            options: vec![
                QuestionOption {
                    key: "comptime_reflection".to_owned(),
                    label: "Rewrite with `const fn` + `core::mem::type_info` / `bstd::comptime` reflection (Recommended)".to_owned(),
                    impact: "Eliminates `syn`, `quote`, `proc-macro2`, and Wasm proc-macro execution completely; emits a `comptime` reflection module.".to_owned(),
                    recommended: true,
                },
                QuestionOption {
                    key: "declarative_macro".to_owned(),
                    label: "Rewrite as zero-dep `macro_rules!` + `const` assertions".to_owned(),
                    impact: "Zero proc-macro deps and works on stable Rust too, at the cost of macro_rules! syntax.".to_owned(),
                    recommended: false,
                },
                QuestionOption {
                    key: "keep_wasm_proc_macro".to_owned(),
                    label: "Keep as custom proc-macro crate (runs sandboxed on Wasm under blaze)".to_owned(),
                    impact: "Keeps `syn`/`quote`/`proc-macro2` in this proc-macro crate only.".to_owned(),
                    recommended: false,
                },
            ],
        });
    }

    // 4b. C / C++ interop crates (`bindgen`, `cc`, `cmake`, `cxx`, `autocxx`, `pkg-config`)
    //     -> `blaze::c::import!` / `blaze::cpp::import!` + `[package.metadata.blaze.c]`.
    for krate in &analysis.crates {
        let Some(cinfo) = &krate.c_interop else { continue };
        let tools_list: Vec<&str> = cinfo.tools.iter().map(String::as_str).collect();
        let macro_name = cinfo.import_macro();
        let lang = if cinfo.is_cpp { "C++" } else { "C" };
        let mut affected = Vec::new();
        affected.push(format!("tools: {}", tools_list.join(", ")));
        if !cinfo.headers.is_empty() {
            affected.push(format!("headers: {}", cinfo.headers.join(", ")));
        }
        if !cinfo.source_files.is_empty() {
            affected.push(format!("sources: {} file(s)", cinfo.source_files.len()));
        }
        if !cinfo.out_dir_includes.is_empty() {
            affected.push(format!("OUT_DIR includes: {}", cinfo.out_dir_includes.join(", ")));
        }
        questions.push(Question {
            id: format!("c_interop:{}", krate.name),
            category: QuestionCategory::CInterop,
            prompt: format!(
                "Crate `{}` uses {lang} build/FFI tooling ({}). Replace `build.rs` + bindgen/cc/cxx with `{macro_name}` + `[package.metadata.blaze.c]`?",
                krate.name,
                tools_list.join(", ")
            ),
            affected,
            options: vec![
                QuestionOption {
                    key: "blaze_c_import".to_owned(),
                    label: format!("Replace with `{macro_name}` + `[package.metadata.blaze.c]` (Recommended)"),
                    impact: format!(
                        "Eliminates `build.rs`, host `libclang`, and `{}`; caches header declarations as records keyed by headers read and compiles {lang} files lazily per unit via blaze's pinned clang.",
                        tools_list.join("`/`")
                    ),
                    recommended: true,
                },
                QuestionOption {
                    key: "keep_build_script".to_owned(),
                    label: "Keep existing `build.rs` (routed through blaze's C mode & bindgen stand-in)".to_owned(),
                    impact: "Keeps current build-dependencies and opaque build script until manually migrated.".to_owned(),
                    recommended: false,
                },
            ],
        });
    }

    // 4c. Primitive obsession -> explicit `bstd::newtype!` (affine Offset/Len, typed IDs, Port, Alignment, Duration).
    if analysis.total_newtype_candidates > 0 {
        let sample: Vec<String> = analysis
            .crates
            .iter()
            .flat_map(|c| {
                c.newtype_candidates.iter().map(move |n| {
                    format!(
                        "{}::{} ({}:{}) `{}: {}` -> `{}`",
                        c.name, n.owner, n.file, n.line, n.name, n.raw_type, n.suggestion
                    )
                })
            })
            .take(10)
            .collect();
        questions.push(Question {
            id: "newtype:promote_primitives".to_owned(),
            category: QuestionCategory::Newtype,
            prompt: format!(
                "Found {} bare-primitive domain parameters/fields (`offset`/`len`, `*_id`, `port`, `*_ms`, `align`). Generate `src/newtypes.rs` with explicit `bstd::newtype!` (affine `Offset`/`Len`, `bstd::id::id!`, Kotlin-style `delegate`, zero blanket `Deref`)?",
                analysis.total_newtype_candidates
            ),
            affected: sample,
            options: vec![
                QuestionOption {
                    key: "emit_newtypes_module".to_owned(),
                    label: "Generate `src/newtypes.rs` using `bstd::newtype!` & `bstd::id::id!` (Recommended)".to_owned(),
                    impact: "Prevents `Offset + Offset` and `Offset`/`Len` mixups at compile time; composes cleanly with `bstd::TypeSet!` for order-independent typed bundles.".to_owned(),
                    recommended: true,
                },
                QuestionOption {
                    key: "report_only".to_owned(),
                    label: "Report only (do not emit `src/newtypes.rs`)".to_owned(),
                    impact: "Lists bare primitive candidates in the analysis report.".to_owned(),
                    recommended: false,
                },
            ],
        });
    }

    // 5. HashMap & Lock policies when used across the workspace.
    let total_maps: usize = analysis.crates.iter().map(|c| c.map_sites).sum();
    if total_maps > 0 {
        questions.push(Question {
            id: "map:hasher".to_owned(),
            category: QuestionCategory::HashMapPolicy,
            prompt: format!(
                "Found {total_maps} hash map/set sites (`HashMap`, `FxHashMap`, `IndexMap`). Which default hasher should `bstd::map::Map` use?"
            ),
            affected: vec![format!("{total_maps} map/set sites")],
            options: vec![
                QuestionOption {
                    key: "fixed_state".to_owned(),
                    label: "`bstd::hash::FixedState` (foldhash, deterministic) (Recommended)".to_owned(),
                    impact: "Fastest for internal keys, deterministic across runs; pass `SipKey` only on attacker-controlled keys.".to_owned(),
                    recommended: true,
                },
                QuestionOption {
                    key: "sip_key".to_owned(),
                    label: "`bstd::hash::SipKey` (DoS-resistant, injected from `Io::sip_key`)".to_owned(),
                    impact: "Safe for untrusted network keys; requires threading `SipKey`.".to_owned(),
                    recommended: false,
                },
            ],
        });
    }

    let total_locks: usize = analysis.crates.iter().map(|c| c.lock_sites).sum();
    if total_locks > 0 {
        questions.push(Question {
            id: "sync:locks".to_owned(),
            category: QuestionCategory::SyncPolicy,
            prompt: format!(
                "Found {total_locks} lock/sync sites (`Mutex`, `RwLock`, `OnceLock`). How should synchronization migrate to `bstd::sync`?"
            ),
            affected: vec![format!("{total_locks} sync sites")],
            options: vec![
                QuestionOption {
                    key: "named_bstd_lock".to_owned(),
                    label: "Replace with named `bstd::sync::Lock` / `ReadLock` (Recommended)".to_owned(),
                    impact: "Mechanical drop-in with lock-wait profiling and ranking; `OnceLock` flagged for explicit init.".to_owned(),
                    recommended: true,
                },
                QuestionOption {
                    key: "bounded_chan_actor".to_owned(),
                    label: "Convert shared-state locks into single-owner state + `bstd::sync::Chan`".to_owned(),
                    impact: "Eliminates lock contention entirely; larger structural refactor.".to_owned(),
                    recommended: false,
                },
            ],
        });
    }

    // 6. External dependencies that need a decision (sorted by number of affected crates, highest first).
    let mut dep_list: Vec<(&String, &alloc::collections::BTreeSet<String>)> =
        analysis.external_dep_users.iter().collect();
    dep_list.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));

    for (pkg, users) in dep_list {
        let mapping = lookup_crate(pkg);
        let needs_question = match mapping {
            Some(m) => matches!(m.disposition, DepDisposition::NeedsDecision | DepDisposition::Unknown),
            None => true,
        };
        if !needs_question {
            continue;
        }
        let target_desc = mapping.map(|m| m.target).unwrap_or("no built-in bstd equivalent yet");
        let summary = mapping
            .map(|m| m.summary)
            .unwrap_or("Not in built-in catalog; decide whether to replace, wrap behind a sans-IO trait, or keep.");

        let recommend_replace = mapping.is_some_and(|m| m.disposition == DepDisposition::NeedsDecision);
        questions.push(Question {
            id: format!("dep:{pkg}"),
            category: QuestionCategory::Dependency,
            prompt: format!(
                "External crate `{pkg}` is used by {} crate(s). Target: `{target_desc}`. {summary}",
                users.len()
            ),
            affected: users.iter().cloned().collect(),
            options: vec![
                QuestionOption {
                    key: "replace_bstd".to_owned(),
                    label: format!("Replace with `{target_desc}` (drives external deps toward 0)"),
                    impact: format!("Removes `{pkg}` from {} Cargo.toml(s) and rewrites imports/call sites.", users.len()),
                    recommended: recommend_replace,
                },
                QuestionOption {
                    key: "wrap_sans_io_trait".to_owned(),
                    label: format!("Isolate `{pkg}` behind a sans-IO capability trait"),
                    impact: "Keeps core logic 0-dep and simulatable while confining the external dep to a thin driver crate.".to_owned(),
                    recommended: false,
                },
                QuestionOption {
                    key: "keep_external".to_owned(),
                    label: format!("Keep `{pkg}` as an external dependency"),
                    impact: format!("Leaves `{pkg}` intact in {} crate(s).", users.len()),
                    recommended: !recommend_replace,
                },
            ],
        });
    }

    // Suppress unused helper warning if OwnVec is only used in JSON helpers.
    let _ = OwnVec::<u8, Alloc<'_>>::new_in;

    questions
}
