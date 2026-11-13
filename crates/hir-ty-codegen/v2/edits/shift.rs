//! The shift property of the edit fuzzer's model: a line added above a function recompiles
//! nothing below it. A position that reaches code is a span of its owner's tables
//! (`blaze_format::SpanRef`: the anchor of the owner's store key and the span's index there),
//! never a line, so moving every item after one of them down `N` lines keeps each function's
//! compile identity and `code_id`, its template key, its compiled bytes, relocations, data and
//! line rows, and compiles nothing: what the link is given anew is the moved bodies' span
//! tables (`linked::tests::a_line_added_above_a_body_places_none_of_it_again`).
//!
//! The control: an edit of a body below the lines compiles it again.

use std::{collections::BTreeMap, sync::Arc};

use blaze_codegen::{CompiledFunction, InstanceKey, driver::Built, memo::Memo};

use super::{
    Edit, ITEMS, Kept, LIBRARIES, Model, build, database,
    identity::{Order, identities},
};

/// The model's file with `lines` blank lines more before the item at `at` of its file order:
/// that item and every one after it move down, and nothing else changes.
fn shifted(model: &Model, at: usize, lines: usize) -> String {
    let mut out = String::new();
    for (place, item) in model.order.iter().enumerate() {
        if place == at {
            out.push_str(&"\n".repeat(lines));
        }
        out.push_str(&model.item(*item));
        out.push_str(&"\n".repeat(usize::from(model.spacing)));
    }
    out
}

/// Each function a build gives the linker, by key.
fn functions(built: &Built) -> BTreeMap<InstanceKey, Arc<CompiledFunction>> {
    // clone: a count of each function, a test's comparison
    built.functions.iter().map(|it| (it.key, it.clone())).collect()
}

/// The template key each function's dev template is kept under.
fn templates(built: &Built) -> BTreeMap<InstanceKey, blaze_cas::Key> {
    built.kernel.iter().map(|it| (it.instance, it.key)).collect()
}

/// Builds the model, moves every item from `at` on down `lines` lines in the same database
/// with the same memo and store, builds again, and returns both builds.
fn build_shifted(at: usize, lines: usize, heap_to_stack: bool) -> (Built, Built, String) {
    let model = Model::new();
    let (mut db, file) = database(&model.source());
    let (memo, mut store) = (Memo::new(), Kept::default());
    let first = build(&db, &memo, &store, blaze_store_check::Setting::ALL, heap_to_stack)
        .unwrap_or_else(|why| panic!("{why}\n{}", model.source()));
    // clone: the first build is returned to compare, and the store keeps its puts too
    store.0.extend(first.puts.iter().map(|it| (it.key, it.bytes.clone())));
    let body = shifted(&model, at, lines);
    db.set_file_text(file, &body);
    let again = build(&db, &memo, &store, blaze_store_check::Setting::OFF, heap_to_stack)
        .unwrap_or_else(|why| panic!("{why}\n{body}"));
    (first, again, body)
}

/// The invariant: `lines` blank lines added above the item at `at` of the model's file compile
/// nothing, and every function, its template key and its codegen input are as they were.
fn a_shift_recompiles_nothing(at: usize, lines: usize, heap_to_stack: bool) {
    let (first, again, body) = build_shifted(at, lines, heap_to_stack);
    let why = || format!("{lines} lines added above item {at} of the file:\n{body}");
    assert_eq!(again.stats.compiled, 0, "compiled again: {}", why());
    assert_eq!(again.inputs, first.inputs, "a codegen input moved: {}", why());
    assert_eq!(templates(&again), templates(&first), "a template key moved: {}", why());
    let (was, now) = (functions(&first), functions(&again));
    assert_eq!(
        was.keys().collect::<Vec<_>>(),
        now.keys().collect::<Vec<_>>(),
        "the functions given the link: {}",
        why()
    );
    for (key, function) in &was {
        let moved = &now[key];
        assert!(
            function.code == moved.code
                && function.relocs == moved.relocs
                && function.data == moved.data
                && function.lines == moved.lines
                && function.side == moved.side,
            "function {key:?}: its bytes, relocations, data, line rows or side table moved: {}",
            why()
        );
    }
    assert_eq!(again.symbols, first.symbols, "a symbol moved: {}", why());
}

/// One line, a few, and many, above the second item, the middle one and the last: each moves
/// what is below it and recompiles none of it.
#[test]
fn a_line_added_above_a_function_recompiles_nothing_below_it() {
    for at in [1, ITEMS / 2, ITEMS - 1] {
        for lines in [1, 3, 200] {
            a_shift_recompiles_nothing(at, lines, false);
        }
    }
    a_shift_recompiles_nothing(1, 7, true);
}

/// The invariant at the level of code identity: moving every item from `at` on down keeps
/// each root's compile identity (debug, moving, release) and `code_id`.
#[test]
fn a_line_added_above_a_function_keeps_its_code_identity() {
    let model = Model::new();
    let source = |body: String| format!("{LIBRARIES}//- /lib.rs crate:edited deps:alloc\n{body}");
    let was = identities(&source(model.body()), Order::TopDown);
    for at in [1, ITEMS - 1] {
        for lines in [1, 40] {
            let now = identities(&source(shifted(&model, at, lines)), Order::TopDown);
            assert_eq!(now, was, "{lines} lines above item {at}");
        }
    }
}

/// A function that reads `line!()` where it runs, below one that lines are added above.
const LINE_READER: &str = "#[rustc_builtin_macro]\nmacro_rules! line {() => {}}\npub fn above() -> u32 {\n    1\n}\npub fn here() -> u32 {\n    line!()\n}\n";

/// [`LINE_READER`] with `lines` blank lines more above `above`, the macro's definition where it
/// was.
fn line_reader_shifted(lines: usize) -> String {
    LINE_READER.replacen("pub fn above", &format!("{}pub fn above", "\n".repeat(lines)), 1)
}

/// The invariant for a body that reads its own line: `line!()` expands to a placeholder and
/// is read where the code runs from the `Location` of the literal, which the link fills from
/// the moved bases (`linked::tests::a_body_reading_its_line_keeps_its_hir_and_key_below_an_inserted_line`
/// checks that line), so lines added above it compile nothing and every function, its
/// template key and its codegen input are as they were.
#[test]
fn a_function_reading_its_line_recompiles_nothing_below_an_inserted_line() {
    let libraries = LIBRARIES.replacen("size_of\n", "size_of, panic_location\n", 1);
    assert_ne!(libraries, LIBRARIES, "the fixture's minicore gains `Location`");
    let source = |body: &str| format!("{libraries}//- /lib.rs crate:edited deps:alloc\n{body}");
    for lines in [1, 4, 90] {
        let (mut db, file) = database(&source(LINE_READER));
        let (memo, mut store) = (Memo::new(), Kept::default());
        let first = build(&db, &memo, &store, blaze_store_check::Setting::ALL, false)
            .unwrap_or_else(|why| panic!("{why}\n{LINE_READER}"));
        // clone: the first build is compared, and the store keeps its puts too
        store.0.extend(first.puts.iter().map(|it| (it.key, it.bytes.clone())));
        let body = line_reader_shifted(lines);
        db.set_file_text(file, &body);
        let again = build(&db, &memo, &store, blaze_store_check::Setting::OFF, false)
            .unwrap_or_else(|why| panic!("{why}\n{body}"));
        assert_eq!(again.stats.compiled, 0, "{lines} lines above `here` compiled again");
        assert_eq!(again.inputs, first.inputs, "a codegen input moved after {lines} lines");
        assert_eq!(templates(&again), templates(&first), "a template key after {lines} lines");
        let (was, now) = (functions(&first), functions(&again));
        assert_eq!(was.keys().collect::<Vec<_>>(), now.keys().collect::<Vec<_>>());
        for (key, function) in &was {
            let moved = &now[key];
            assert!(
                function.code == moved.code
                    && function.relocs == moved.relocs
                    && function.data == moved.data
                    && function.lines == moved.lines
                    && function.side == moved.side,
                "function {key:?} moved after {lines} lines"
            );
        }
    }
}

/// The control: the same build with the shift and an edit of a body below it (`mix`'s) does
/// compile again, so the property above is not met by a build that compiles nothing at all.
#[test]
fn the_control_a_body_edited_below_the_lines_compiles_again() {
    let mut model = Model::new();
    let (mut db, file) = database(&model.source());
    let (memo, mut store) = (Memo::new(), Kept::default());
    let first = build(&db, &memo, &store, blaze_store_check::Setting::OFF, false)
        .unwrap_or_else(|why| panic!("{why}"));
    store.0.extend(first.puts.into_iter().map(|it| (it.key, it.bytes)));
    model.apply(Edit::ChangeInlined(model.inlined + 1));
    db.set_file_text(file, &shifted(&model, 1, 3));
    let again = build(&db, &memo, &store, blaze_store_check::Setting::OFF, false)
        .unwrap_or_else(|why| panic!("{why}"));
    assert!(again.stats.compiled > 0, "an edited body was not compiled again");
}
