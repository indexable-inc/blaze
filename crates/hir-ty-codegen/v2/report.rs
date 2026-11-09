//! What a function's optimized code still does in its loops, by statement: the report lint
//! `blaze::vecreport` prints (docs/v2/plan/design/vecreport.md). LLVM's units built
//! with [`blaze_codegen::settings::Spec::report`] have one; it is read from the code LLVM is
//! about to write ([`crate::llvm::report`]), never guessed from MIR.
//!
//! A function's rows are kept after its side table ([`super::side`]), in postcard, with
//! their own [`VERSION`]: a function without a report has the side table it had, byte for
//! byte, and one with only a report has an empty table before it. [`of_side`] reads them.
//!
//! The stencil tier reports nothing: a stencil keeps every check its MIR has (nothing proves
//! a range) and vectorizes no loop, which `blaze::checkfree`'s MIR scan
//! (`hir_ty_mir::mir::checks`) already lists.

use blaze_codegen::InstanceKey;
use blaze_format::SpanRef;
use serde_derive::{Deserialize, Serialize};

/// The encoding's version: a change to what is below is a new one.
pub(crate) const VERSION: u32 = 1;

/// One finding of one function.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Row {
    /// The statement of its code: the panic path's call, or the loop the vectorizer speaks of.
    /// `None` for code of no statement, which is told at the function.
    pub at: Option<SpanRef>,
    /// The statement its loop starts at, where known.
    pub loop_at: Option<SpanRef>,
    pub what: What,
    pub heat: Heat,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum What {
    /// A branch out of the loop to a path that panics survived every pass: a bounds check,
    /// an overflow check, an `unwrap` LLVM could not prove dead. The path calls `callee`.
    Kept { callee: Callee },
    /// The loop vectorizer left the loop scalar: what it said (its "missed" remark, without
    /// "loop not vectorized"), and why (its "analysis" remarks, in its order).
    Scalar { said: String, why: Vec<String> },
}

/// What a panic path calls.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Callee {
    /// A function of the build: `core::panicking::panic_bounds_check` and its kin, by key,
    /// which the build's debug names name.
    Function(InstanceKey),
    /// A symbol outside blaze's code.
    Extern(String),
    /// Nothing: the path traps.
    Trap,
    /// A function through a pointer.
    Pointer,
}

/// How hot the finding's loop is, by the profile the function was built with.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Heat {
    /// Built without one: every loop is reported.
    Unprofiled,
    /// Built with one, and no count reaches the loop. Not hot: reported only without a
    /// profile.
    Uncounted,
    /// The loop header's count, not hot by the profile's summary.
    Cold(u64),
    /// The loop header's count, hot by the profile's summary
    /// (LLVM's `ProfileSummaryInfo::isHotCount`).
    Hot(u64),
}

impl Heat {
    /// Whether the row is reported: every row without a profile, only hot ones with one.
    pub fn reported(self) -> bool {
        matches!(self, Heat::Unprofiled | Heat::Hot(_))
    }
}

/// The report as stored.
#[derive(Serialize, Deserialize)]
struct Section {
    version: u32,
    rows: Vec<Row>,
}

/// `side`, the function's side table and its content ID, with `rows` after it: `side` as it
/// is when there are none, an empty table before them when there is no table.
pub(crate) fn with_side(
    side: Option<(Vec<u8>, blaze_ids::ContentId)>,
    rows: Vec<Row>,
) -> Result<Option<(Vec<u8>, blaze_ids::ContentId)>, String> {
    if rows.is_empty() {
        return Ok(side);
    }
    let mut bytes = match side {
        Some((bytes, _)) => bytes,
        None => super::side::empty()?,
    };
    let section = Section { version: VERSION, rows };
    // alloc: the error's message
    bytes = postcard::to_extend(&section, bytes).map_err(|e| e.to_string())?;
    let id = super::side::id(&bytes);
    Ok(Some((bytes, id)))
}

/// The rows of the side table `bytes`: none for one without a report.
pub fn of_side(bytes: &[u8]) -> Result<Vec<Row>, String> {
    let rest = super::side::after(bytes)?;
    if rest.is_empty() {
        // alloc: empty, no allocation
        return Ok(Vec::new());
    }
    // alloc: the error's message
    let (section, left) =
        postcard::take_from_bytes::<Section>(rest).map_err(|e| format!("its report: {e}"))?;
    if section.version != VERSION {
        // alloc: the error's message
        return Err(format!("a report of version {}, not {VERSION}", section.version));
    }
    if !left.is_empty() {
        // alloc: the error's message
        return Err(format!("{} bytes after its report", left.len()));
    }
    Ok(section.rows)
}

/// The rows of LLVM's findings `found` in a function whose statements are `spans` (a line is
/// a statement plus one: [`crate::debuginfo::LineRow`]), whose function requests are placed
/// by `funcs` among `refs`. LLVM's order is kept for a loop's reasons; the rows are then
/// sorted, and the same row twice is one. A line past the statements, or a call of a request
/// nothing places, is an error naming it: never a row dropped.
#[cfg(feature = "llvm")]
pub(crate) fn rows(
    found: Vec<crate::llvm::report::Raw>,
    spans: &[SpanRef],
    funcs: &[u32],
    refs: &[blaze_codegen::Target],
) -> Result<Vec<Row>, String> {
    use crate::llvm::report::{Called, Found, Heat as Seen};

    let at = |line: u32| -> Result<Option<SpanRef>, String> {
        let Some(statement) = crate::debuginfo::LineRow::span_of_line(line) else {
            return Ok(None);
        };
        match spans.get(statement as usize) {
            Some(span) => Ok(Some(*span)),
            // alloc: the error's message
            None => Err(format!(
                "LLVM reports line {line}, of statement {statement}, and it has {}",
                spans.len()
            )),
        }
    };
    let heat = |seen: Seen| match seen {
        Seen::Unprofiled => Heat::Unprofiled,
        Seen::Uncounted => Heat::Uncounted,
        Seen::Cold(count) => Heat::Cold(count),
        Seen::Hot(count) => Heat::Hot(count),
    };
    // alloc: once per function reported, a row a finding
    let mut rows = Vec::with_capacity(found.len());
    // The reasons LLVM gave before the missed remark they are of, with where they were.
    // alloc: once per function reported, a reason a remark
    let mut why: Vec<(u32, u32, Seen, String)> = Vec::new();
    for raw in found {
        match raw.found {
            Found::Kept { callee } => {
                let callee = match callee {
                    Called::Request(request) => {
                        let target =
                            funcs.get(request as usize).and_then(|&it| refs.get(it as usize));
                        match target {
                            Some(
                                blaze_codegen::Target::Function(key)
                                | blaze_codegen::Target::Direct(key),
                            ) => Callee::Function(*key),
                            // clone: the outside symbol's name, once a row
                            Some(
                                blaze_codegen::Target::Extern(name)
                                | blaze_codegen::Target::Weak(name),
                            ) => Callee::Extern(name.clone()),
                            // alloc: the error's message
                            Some(other) => {
                                return Err(format!(
                                    "a panic path calls request {request}, a {other:?}"
                                ));
                            }
                            // alloc: the error's message
                            None => {
                                return Err(format!(
                                    "a panic path calls request {request}, which nothing places"
                                ));
                            }
                        }
                    }
                    Called::Named(name) => Callee::Extern(name),
                    Called::Trap => Callee::Trap,
                    Called::Pointer => Callee::Pointer,
                };
                rows.push(Row {
                    at: at(raw.line)?,
                    loop_at: at(raw.loop_line)?,
                    what: What::Kept { callee },
                    heat: heat(raw.heat),
                });
            }
            // Interleaving is the vectorizer's other half: a loop it left scalar says so too.
            Found::Analysis { name, .. } if name.starts_with("Interleav") => {}
            Found::Missed { name, .. } if name.starts_with("Interleav") => {}
            Found::Analysis { message, .. } => {
                why.push((raw.line, raw.loop_line, raw.heat, message))
            }
            Found::Missed { message, .. } => {
                let said = message.strip_prefix("loop not vectorized").unwrap_or(&message);
                let said = said.trim_start_matches(':').trim();
                // alloc: the reasons of one loop, kept in its row
                let reasons = why.drain(..).map(|it| tidy(&it.3)).collect();
                rows.push(Row {
                    at: at(raw.line)?,
                    loop_at: at(raw.loop_line)?,
                    // alloc: the remark's text, kept in its row
                    what: What::Scalar { said: said.to_owned(), why: reasons },
                    heat: heat(raw.heat),
                });
            }
        }
    }
    // Reasons no missed remark followed: a loop LLVM spoke of without saying it left it.
    if let Some(&(line, loop_line, seen, _)) = why.first() {
        // alloc: the reasons, kept in their row
        let reasons = why.iter().map(|it| tidy(&it.3)).collect();
        rows.push(Row {
            at: at(line)?,
            loop_at: at(loop_line)?,
            what: What::Scalar { said: String::new(), why: reasons },
            heat: heat(seen),
        });
    }
    rows.sort_unstable();
    rows.dedup();
    Ok(rows)
}

/// A reason without the prefix LLVM gives each of them.
#[cfg(feature = "llvm")]
fn tidy(message: &str) -> String {
    let said = message.strip_prefix("loop not vectorized").unwrap_or(message);
    // alloc: the remark's text, kept in its row
    said.trim_start_matches(':').trim().to_owned()
}

/// One finding as it is printed: at a position of the source, never of the IR.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Reported {
    /// The file as the program knows it (`file!()`): relative to its package's root. Empty
    /// for code of no statement, told at its function with no position.
    pub file: String,
    /// 1-based, as `core::panic::Location` has them.
    pub line: u32,
    pub column: u32,
    pub message: String,
    /// The function whose code it is, by its debug name.
    pub function: String,
}

impl super::HirFrontend<'_> {
    /// What `reports`, each function's rows by its key, say, at the positions of `built`'s
    /// span tables: only rows [`Heat::reported`] keeps (with a profile, the hot loops alone),
    /// sorted by position and message, the same finding of two instances once. A row whose
    /// statement no table of the build has is an error naming the function.
    pub fn vecreport(
        &self,
        built: &blaze_codegen::driver::Built,
        reports: &[(InstanceKey, Vec<Row>)],
    ) -> Result<Vec<Reported>, blaze_codegen::CodegenError> {
        let error = |what: String| blaze_codegen::CodegenError {
            unit: InstanceKey(blaze_ids::ContentId::default()),
            item: blaze_items::ItemKey(blaze_ids::ContentId::default()),
            what,
            front: None,
        };
        self.with(|db| reported(db, &self.hashes, built, reports)).map_err(error)
    }
}

fn reported(
    db: &base_db::Db,
    hashes: &super::HashSource,
    built: &blaze_codegen::driver::Built,
    reports: &[(InstanceKey, Vec<Row>)],
) -> Result<Vec<Reported>, String> {
    use super::Hashes as _;
    use blaze_hash::HashMap;
    use std::alloc::Global;

    let name_of = |key: InstanceKey| {
        built.debug_names.iter().find(|it| it.0 == key).map_or("<unnamed>", |it| it.1.as_str())
    };
    // Which body each anchor a row names is of, and which of its tables.
    let mut wanted: HashMap<blaze_ids::ContentId, Option<(hir_def::DefWithBodyId, usize)>, Global> =
        HashMap::new_in(Global);
    for (_, rows) in reports {
        for row in rows.iter().filter(|it| it.heat.reported()) {
            if let Some(at) = row.at.or(row.loop_at) {
                wanted.insert(at.anchor, None);
            }
        }
    }
    if wanted.is_empty() {
        // alloc: empty, no allocation
        return Ok(Vec::new());
    }
    let mut owners: blaze_hash::HashSet<hir_def::DefWithBodyId, Global> =
        blaze_hash::HashSet::new_in(Global);
    for &item in &built.items {
        use blaze_items::{ItemId, key::Resolved};
        let owner = match blaze_items::resolve(db, item) {
            Some(Resolved::Item(ItemId::Function(func))) => {
                hir_def::DefWithBodyId::FunctionId(func)
            }
            Some(Resolved::Item(ItemId::Static(it))) => hir_def::DefWithBodyId::StaticId(it),
            Some(Resolved::Item(ItemId::Const(it))) => hir_def::DefWithBodyId::ConstId(it),
            Some(Resolved::Variant(it)) => hir_def::DefWithBodyId::VariantId(it),
            Some(Resolved::Closure {
                store: hir_def::ExpressionStoreOwnerId::Body(owner), ..
            }) => owner,
            _ => continue,
        };
        owners.insert(owner);
    }
    // Sorted: which owner a shared anchor is found under first does not hang on hashing.
    // alloc: once per report, the build's bodies
    let mut owners: Vec<_> = owners.into_iter().collect();
    owners.sort_unstable();
    for owner in owners {
        for (table, anchor) in hashes.anchors(db, owner)?.into_iter().enumerate() {
            if let Some(slot @ None) = wanted.get_mut(&anchor) {
                *slot = Some((owner, table));
            }
        }
    }
    let mut files: HashMap<base_db::FileId, String, Global> = HashMap::new_in(Global);
    // alloc: once per report, a finding a row
    let mut out = Vec::new();
    for &(key, ref rows) in reports {
        let function = name_of(key);
        for row in rows.iter().filter(|it| it.heat.reported()) {
            // Code of no statement (a line 0 LLVM made): told at the function, by name alone.
            let Some(at) = row.at.or(row.loop_at) else {
                out.push(Reported {
                    file: String::new(),
                    line: 0,
                    column: 0,
                    message: message(row, &|key| name_of(key)),
                    // alloc: the function's name, once a finding
                    function: function.to_owned(),
                });
                continue;
            };
            let Some(Some((owner, table))) = wanted.get(&at.anchor).copied() else {
                // alloc: the error's message
                return Err(format!(
                    "`{function}`'s finding names span table {:?}, which no body of the build has",
                    at.anchor
                ));
            };
            let body = super::linked::positions(db, owner).as_ref().map_err(Clone::clone)?;
            let start = body.starts[table] as usize;
            let end = body.starts[table + 1] as usize;
            let Some(&(file, line, column)) = body.rows[start..end].get(at.index as usize) else {
                // alloc: the error's message
                return Err(format!(
                    "`{function}`'s finding is span {} of a table of {}",
                    at.index,
                    end - start
                ));
            };
            if !files.contains_key(&file) {
                files.insert(file, hashes.file_name(db, file)?);
            }
            out.push(Reported {
                // clone: the file's name, once a finding
                file: files[&file].clone(),
                line,
                column,
                message: message(row, &|key| name_of(key)),
                // alloc: the function's name, once a finding
                function: function.to_owned(),
            });
        }
    }
    out.sort_unstable();
    // The same finding of two instances of one generic function: one line.
    out.dedup_by(|a, b| {
        (&a.file, a.line, a.column, &a.message) == (&b.file, b.line, b.column, &b.message)
    });
    Ok(out)
}

/// What a row says: which check a kept panic path is, by its callee's name, and the loop's
/// count where a profile gave one.
fn message<'a>(row: &Row, name_of: &dyn Fn(InstanceKey) -> &'a str) -> String {
    let count = match row.heat {
        // alloc: a finding's text
        Heat::Hot(count) => format!(" (the loop runs {count} times in the profile)"),
        Heat::Unprofiled | Heat::Uncounted | Heat::Cold(_) => String::new(),
    };
    match &row.what {
        What::Kept { callee } => {
            let (kind, name) = match callee {
                Callee::Function(key) => {
                    let name = name_of(*key);
                    (check_of(name), name)
                }
                Callee::Extern(name) => (check_of(name), name.as_str()),
                Callee::Trap => ("a trap", "llvm.trap"),
                Callee::Pointer => ("a panic", "a function pointer"),
            };
            // alloc: a finding's text
            format!("{kind} survived optimization inside this loop: its path calls `{name}`{count}")
        }
        What::Scalar { said, why } => {
            let said = if said.is_empty() { "" } else { said.as_str() };
            let mut text = match said {
                // alloc: a finding's text
                "" => format!("this loop is not vectorized{count}"),
                // alloc: a finding's text
                said => format!("this loop is not vectorized: {said}{count}"),
            };
            for reason in why {
                text.push_str("; ");
                text.push_str(reason);
            }
            text
        }
    }
}

/// Which check a panic path is, by the name of what it calls.
fn check_of(name: &str) -> &'static str {
    if name.contains("panic_bounds_check")
        || name.contains("slice_index")
        || name.contains("slice_start_index")
        || name.contains("slice_end_index")
        || name.contains("str_index")
    {
        "a bounds check"
    } else if name.contains("div_by_zero") || name.contains("rem_by_zero") {
        "a division-by-zero check"
    } else if name.contains("overflow") {
        "an overflow check"
    } else if name.contains("unwrap_failed") || name.contains("expect_failed") {
        "an `unwrap`"
    } else {
        "a panic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(index: u32) -> SpanRef {
        SpanRef { anchor: blaze_ids::ContentId::of("test", b"anchor"), index }
    }

    #[test]
    fn a_side_table_without_a_report_is_unchanged() {
        let side = Some((vec![4, 0, 0, 0, 0], blaze_ids::ContentId::of("test", b"side")));
        assert_eq!(with_side(side.clone(), Vec::new()).unwrap(), side);
    }

    #[test]
    fn a_report_reads_back_after_its_table() {
        let rows = vec![Row {
            at: Some(span(3)),
            loop_at: Some(span(1)),
            what: What::Scalar {
                said: "call instruction cannot be vectorized".into(),
                why: vec!["x".into()],
            },
            heat: Heat::Hot(9000),
        }];
        let (bytes, _) = with_side(None, rows.clone()).unwrap().unwrap();
        super::super::side::decode(&bytes).unwrap();
        assert_eq!(of_side(&bytes).unwrap(), rows);
        assert_eq!(of_side(&super::super::side::empty().unwrap()).unwrap(), Vec::new());
    }
}
