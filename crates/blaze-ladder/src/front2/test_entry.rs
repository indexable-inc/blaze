//! What a TEST FUNCTION's entry is (docs/v2/plan/design/test.md, "End state", piece 8): fixed
//! here, by its signature alone, before the suite's walk starts.
//!
//! A test is a row of its unit's tests record (blaze-scope tests_record.rs); no harness is
//! written as Rust source for blaze to compile: the launcher calls row n's ENTRY. What that entry
//! is follows from the Reference, "Testing attributes", "The `test` attribute":
//!
//! - "The test attribute may only be applied to free functions that are monomorphic, that take
//!   no arguments, and where the return type implements the Termination trait." (the pinned
//!   copy, `share/doc/rust/html/reference/attributes/testing.html`, rule
//!   `attributes.testing.test.allowed-positions`); its note: "Some of types that implement the
//!   Termination trait include: `()`, `Result<T, E> where T: Termination, E: Debug`".
//! - The 1.88 copy of the same page: "The test harness calls the returned value's report
//!   method, and classifies the test as passed or failed depending on whether the resulting
//!   ExitCode represents successful termination."
//!
//! So an entry is one of two forms ([`EntryShape`]):
//!
//! - a test returning `()` is its own entry: the launcher calls the fn; returning is a pass;
//! - a test returning any other `T: Termination` is entered through a SHIM instance (the same
//!   mechanism as an `Fn*` shim, crate::fn_shim: an instance the builder makes, keyed apart from
//!   the fn's own instances) that calls the test, then `<T as Termination>::report` on its value,
//!   and returns the `ExitCode`. The `report` fn is found HERE, by the one impl index the checker
//!   reads ([`super::check::answer_impls`] at the return type's shape), and is a root of the
//!   suite's walk, so it is tabled and in the test's reach.
//!
//! `#[should_panic]` and `#[ignore]` are data on the row (the runner judges them); an attribute
//! macro's test (`#[tokio::test]`) is the ordinary fn its expansion wrote by the time it is here.
//!
//! Refused by name ([`TestEntryRefusal`]), never guessed: a member of an impl or trait, a generic
//! fn, a fn taking arguments, an `async fn` (its return type is its own future: the Reference
//! lists no `Termination` for a future, and rustc's own refusal says "async functions cannot be
//! used for tests"), any other opaque return, a return type with no `Termination` impl or with
//! more than one, an impl with no `report`.

use blaze_id::ItemKey as LibKey;
use blaze_typeck::{ItemKey, PNode, PTy};

use super::check::{Records, answer_impls};
use super::keys::{MemberKind, opaque_key};

/// The lang item std's `Termination` trait is declared as (`#[lang = "termination"]`).
pub const TERMINATION_LANG: &str = "termination";

/// How the launcher enters one test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryShape {
    /// The test returns `()`: the fn is its own entry.
    Direct,
    /// The test returns `output` (a type of the records' table, `Store::tys`, which the
    /// program's table starts with): its entry is the report shim calling the test, then
    /// `report` (the checker key of the selected impl's `report` fn) on its value.
    Report { output: PTy, report: ItemKey },
}

/// A test function whose entry this front end does not make, by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestEntryRefusal {
    /// The store holds no head for the test fn (its lowering did not make one).
    NoHead { test: LibKey },
    /// A member of an impl or a trait: "free functions" only.
    Member { test: LibKey },
    /// Type or const generic parameters (its own or its container's): "monomorphic" only.
    Generic { test: LibKey, params: u32 },
    /// "that take no arguments".
    TakesArguments { test: LibKey, count: u32 },
    /// An `async fn`: its return type is its future, which implements no `Termination`.
    Async { test: LibKey },
    /// A return-position `impl Trait` other than an `async fn`'s own future.
    OpaqueReturn { test: LibKey },
    /// The program's crates declare no `#[lang = "termination"]` trait (or its key clashed).
    NoTermination,
    /// The return type's shape has no `Termination` impl, or its question stayed a gap.
    NoReportImpl { test: LibKey },
    /// More than one `Termination` impl answers the return type's shape.
    TwoReportImpls { test: LibKey, first: ItemKey, second: ItemKey },
    /// The selected impl files no `report` fn.
    NoReportFn { test: LibKey, impl_: ItemKey },
    /// The return type could not be read in the records' table.
    Malformed { test: LibKey },
}

impl core::fmt::Display for TestEntryRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const RULE: &str = "the Reference, `attributes.testing.test.allowed-positions`";
        match self {
            TestEntryRefusal::NoHead { test } => write!(f, "the test fn {test:?} has no head in front2's store"),
            TestEntryRefusal::Member { test } => write!(f, "the test fn {test:?} is a member of an impl or trait; a test is a free fn ({RULE})"),
            TestEntryRefusal::Generic { test, params } => write!(f, "the test fn {test:?} has {params} generic parameters; a test is monomorphic ({RULE})"),
            TestEntryRefusal::TakesArguments { test, count } => write!(f, "the test fn {test:?} takes {count} arguments; a test takes none ({RULE})"),
            TestEntryRefusal::Async { test } => write!(f, "the test fn {test:?} is an `async fn`: its future implements no `Termination` ({RULE})"),
            TestEntryRefusal::OpaqueReturn { test } => write!(f, "the test fn {test:?} returns an `impl Trait`; its `Termination` impl is not selected through an opaque type (front2's)"),
            TestEntryRefusal::NoTermination => write!(f, "no crate of the program declares `#[lang = \"{TERMINATION_LANG}\"]`, so a test returning a value has no report"),
            TestEntryRefusal::NoReportImpl { test } => write!(f, "the test fn {test:?}'s return type has no `Termination` impl ({RULE})"),
            TestEntryRefusal::TwoReportImpls { test, first, second } => {
                write!(f, "the test fn {test:?}'s return type has two `Termination` impls, {first:?} and {second:?} (rule 21: one answer per question)")
            }
            TestEntryRefusal::NoReportFn { test, impl_ } => write!(f, "the `Termination` impl {impl_:?} selected for the test fn {test:?} files no `report` fn"),
            TestEntryRefusal::Malformed { test } => write!(f, "the test fn {test:?}'s return type is not in the records' table"),
        }
    }
}

/// What a test fn's HEAD alone says of its entry ([`head_shape`]): the half of the rule that
/// needs no impl index, which every caller shares (rule 30): [`entry_shape`] for the suite, and
/// blaze-ladder run/special/test_roots.rs for one test's roots under the lazy parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadShape {
    /// It returns `()`: the fn is its own entry ([`EntryShape::Direct`]).
    Unit,
    /// It returns `output`, a non-opaque type: its entry is the report shim, whose `report` fn
    /// [`entry_shape`] selects.
    Returns(PTy),
}

/// The head-only half of the entry rule of the test fn `test` (library key; `key` its checker
/// key), whose head is in `store`: a free, monomorphic fn taking no arguments, returning `()`
/// or a type that is not opaque. Each refusal by name.
pub fn head_shape(store: &super::heads::Store<'_>, test: LibKey, key: ItemKey) -> Result<HeadShape, TestEntryRefusal> {
    let (output, params, inputs, member) = {
        let head = store.fns.get(&key).ok_or(TestEntryRefusal::NoHead { test })?;
        (head.output, head.parent + head.own, u32::try_from(head.inputs.len()).unwrap_or(u32::MAX), head.container.is_some())
    };
    if member {
        return Err(TestEntryRefusal::Member { test });
    }
    if params != 0 {
        return Err(TestEntryRefusal::Generic { test, params });
    }
    if inputs != 0 {
        return Err(TestEntryRefusal::TakesArguments { test, count: inputs });
    }
    let node = store.tys.view().node(output).map_err(|_| TestEntryRefusal::Malformed { test })?;
    match node {
        PNode::Tuple(list) => {
            let unit = store.tys.view().list(list).map_err(|_| TestEntryRefusal::Malformed { test })?.is_empty();
            if unit {
                return Ok(HeadShape::Unit);
            }
        }
        // An `async fn`'s own future is its opaque type number 0 (keys.rs `opaque_key`).
        PNode::Opaque(opaque, _) if opaque == opaque_key(key, 0) => return Err(TestEntryRefusal::Async { test }),
        PNode::Opaque(..) => return Err(TestEntryRefusal::OpaqueReturn { test }),
        _ => {}
    }
    Ok(HeadShape::Returns(output))
}

/// The entry of the test fn `test` (library key; `key` its checker key), whose body was just
/// lowered (so its head is in the store): read from its head alone ([`head_shape`]), the
/// `report` fn resolved through the checker's own impl index.
pub fn entry_shape(r: &mut Records<'_, '_>, test: LibKey, key: ItemKey) -> Result<EntryShape, TestEntryRefusal> {
    let output = match head_shape(&*r.store, test, key)? {
        HeadShape::Unit => return Ok(EntryShape::Direct),
        HeadShape::Returns(output) => output,
    };
    let lib = r.index.lang(TERMINATION_LANG).ok_or(TestEntryRefusal::NoTermination)?;
    let termination = r.keys.item(lib).map_err(|_| TestEntryRefusal::NoTermination)?;
    let shape = super::buckets::shape_of(r.store.tys.view(), output).ok_or(TestEntryRefusal::Malformed { test })?;
    let impls = answer_impls(r, termination, shape).ok_or(TestEntryRefusal::NoReportImpl { test })?;
    let impl_ = match impls.as_slice() {
        [] => return Err(TestEntryRefusal::NoReportImpl { test }),
        [one] => *one,
        [first, second, ..] => return Err(TestEntryRefusal::TwoReportImpls { test, first: *first, second: *second }),
    };
    let report = r.keys.filed_member(impl_, MemberKind::Fn, "report").ok_or(TestEntryRefusal::NoReportFn { test, impl_ })?;
    Ok(EntryShape::Report { output, report })
}
