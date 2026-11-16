//! The shapes in place of what std makes easy to get slow (docs/v2/plan/design/std.md): `Lists`
//! against `Vec<Vec<T>>` and a bucketed map, `Texts` and `SortedTexts` against `Vec<String>` and
//! a `BTreeSet<String>`, and `get_or_insert_with` on both ordered maps against std's `BTreeMap`,
//! counting the owned keys made (one per miss, none per hit). Every case runs under the counting
//! allocator, so every block is freed once, and with tokens, so every item is dropped once.

use super::*;
use crate::{
    BTreeMap as OwnBTreeMap, DirId, Lists, PathId, Paths, PathsView, Segs, SharedSegs, SortedTexts, Texts,
    paths::PathColumns,
};

// ---------------------------------------------------------------------------------------------
// Lists.

fn lists_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    tokens::fresh();
    {
        let mut ours: Lists<Token, &Counting> = Lists::new_in(&alloc);
        let mut model: StdVec<StdVec<u64>> = StdVec::new();
        for _ in 0..tape.index(sized(48, 12), "steps") {
            match tape.below(6, "op") {
                0..=1 => {
                    let n = tape.below(9, "items");
                    let row: StdVec<u64> = (0..n).map(|_| tape.below(100, "item")).collect();
                    let at = ours.push_row(row.iter().map(|&v| Token::new(v)));
                    if at != model.len() {
                        return Err(Broke::Oracle(format!("push_row gave {at}, model {}", model.len())));
                    }
                    model.push(row);
                }
                2 => {
                    let n = tape.below(9, "items");
                    let row: StdVec<u64> = (0..n).map(|_| tape.below(100, "item")).collect();
                    let mut open = ours.open_row();
                    for &v in &row {
                        open.push(Token::new(v));
                    }
                    same("open row's items", open.items().iter().map(|t| t.value), row.iter().copied())?;
                    drop(open);
                    model.push(row);
                }
                3 => {
                    let popped = ours.pop_row();
                    if popped != model.pop().is_some() {
                        return Err(Broke::Oracle(format!("pop_row gave {popped}")));
                    }
                }
                4 if !model.is_empty() => {
                    let i = tape.index(model.len(), "row");
                    for t in ours.row_mut(i) {
                        t.value += 1;
                    }
                    for v in &mut model[i] {
                        *v += 1;
                    }
                }
                _ => {
                    let i = tape.index(model.len() + 1, "row");
                    same(
                        "get",
                        ours.get(i).map(|row| row.iter().map(|t| t.value).collect::<StdVec<_>>()).into_iter(),
                        model.get(i).cloned().into_iter(),
                    )?;
                }
            }
            if ours.len() != model.len() || ours.items_len() != model.iter().map(StdVec::len).sum::<usize>() {
                return Err(Broke::Oracle(format!("{} rows, model {}", ours.len(), model.len())));
            }
            same(
                "the rows",
                ours.iter().map(|row| row.iter().map(|t| t.value).collect::<StdVec<_>>()),
                model.iter().cloned(),
            )?;
            for (i, row) in model.iter().enumerate() {
                same("row", ours.row(i).iter().map(|t| t.value), row.iter().copied())?;
            }
            tokens::check(model.iter().map(StdVec::len).sum())?;
        }
    }
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn lists_match_vec_of_vecs() {
    blaze_sim::check(cases(300), lists_case);
}

fn lists_pairs_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    tokens::fresh();
    {
        let rows = tape.index(sized(12, 5), "rows");
        let mut pairs: Vec<(usize, Token), &Counting> = Vec::new_in(&alloc);
        let mut model: StdVec<StdVec<u64>> = (0..rows).map(|_| StdVec::new()).collect();
        if rows > 0 {
            for _ in 0..tape.index(sized(40, 12), "pairs") {
                let row = tape.index(rows, "row");
                let v = tape.below(100, "item");
                pairs.push((row, Token::new(v)));
                model[row].push(v);
            }
        }
        let ours: Lists<Token, &Counting> = Lists::from_pairs_in(rows, pairs, &alloc);
        same(
            "rows from pairs",
            ours.iter().map(|row| row.iter().map(|t| t.value).collect::<StdVec<_>>()),
            model.iter().cloned(),
        )?;
        tokens::check(model.iter().map(StdVec::len).sum())?;
    }
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn lists_from_pairs_match_buckets() {
    blaze_sim::check(cases(300), lists_pairs_case);
}

/// A pair naming a row past the end panics before any item moves, and every item is dropped once.
#[test]
fn lists_from_pairs_refuses_a_row_past_the_end() -> Result<(), Broke> {
    let alloc = Counting::new();
    tokens::fresh();
    let mut pairs: Vec<(usize, Token), &Counting> = Vec::new_in(&alloc);
    pairs.push((0, Token::new(1)));
    pairs.push((3, Token::new(2)));
    panics(|| drop(Lists::<Token, &Counting>::from_pairs_in(3, pairs, &alloc)))?;
    tokens::check(0)?;
    alloc.check()
}

/// A panic while a row is open leaves the items pushed so far as a whole row.
#[test]
fn lists_open_row_closes_on_a_panic() -> Result<(), Broke> {
    let alloc = Counting::new();
    tokens::fresh();
    {
        let mut ours: Lists<Token, &Counting> = Lists::new_in(&alloc);
        ours.push_row([Token::new(1)]);
        panics(|| {
            let mut open = ours.open_row();
            open.push(Token::new(2));
            open.push(Token::new(3));
            panic!("a planned panic while the row is open");
        })?;
        same("the rows", ours.iter().map(|row| row.iter().map(|t| t.value).collect::<StdVec<_>>()), [
            std::vec![1],
            std::vec![2, 3],
        ]
        .into_iter())?;
        tokens::check(3)?;
    }
    tokens::check(0)?;
    alloc.check()
}

// ---------------------------------------------------------------------------------------------
// Texts and SortedTexts.

/// A short string from the tape, with one- to four-byte characters, so a cut inside a character
/// would show.
fn short_text(tape: &Tape) -> String {
    const CHARS: [char; 6] = ['a', 'b', 'z', 'é', '字', '🦀'];
    (0..tape.below(6, "length")).map(|_| CHARS[tape.index(CHARS.len(), "char")]).collect()
}

fn texts_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut ours: Texts<&Counting> = Texts::new_in(&alloc);
        let mut model: StdVec<String> = StdVec::new();
        for _ in 0..tape.index(sized(64, 16), "steps") {
            match tape.below(5, "op") {
                0..=2 => {
                    let text = short_text(tape);
                    let at = ours.push(&text);
                    if at != model.len() {
                        return Err(Broke::Oracle(format!("push gave {at}, model {}", model.len())));
                    }
                    model.push(text);
                }
                3 => {
                    if ours.pop() != model.pop().is_some() {
                        return Err(Broke::Oracle(String::from("pop disagrees")));
                    }
                }
                _ => {
                    let i = tape.index(model.len() + 1, "index");
                    same("get", ours.get(i).into_iter(), model.get(i).map(String::as_str).into_iter())?;
                }
            }
            same("the texts", ours.iter(), model.iter().map(String::as_str))?;
            if ours.bytes_len() != model.iter().map(String::len).sum::<usize>() {
                return Err(Broke::Oracle(format!("{} bytes held", ours.bytes_len())));
            }
        }
        let sorted: SortedTexts<&Counting> = SortedTexts::from_texts_in(&ours, &alloc);
        let model_set: BTreeSet<String> = model.iter().cloned().collect();
        same("sorted", sorted.iter(), model_set.iter().map(String::as_str))?;
        for _ in 0..sized(16, 4) {
            let probe = short_text(tape);
            let found = sorted.index_of(&probe).map(|i| sorted.text(i));
            same("lookup", found.into_iter(), model_set.get(&probe).map(String::as_str).into_iter())?;
            let place = sorted.find(&probe).unwrap_or_else(|at| at);
            let model_place = model_set.iter().take_while(|s| **s < probe).count();
            if place != model_place {
                return Err(Broke::Oracle(format!("{probe:?} placed at {place}, model {model_place}")));
            }
        }
    }
    alloc.check()
}

#[test]
fn texts_match_vec_of_strings() {
    blaze_sim::check(cases(300), texts_case);
}

// ---------------------------------------------------------------------------------------------
// `get_or_insert_with` on the ordered maps.

fn get_or_insert_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut sorted: SortedMap<crate::String<&Counting>, u64, &Counting> = SortedMap::new_in(&alloc);
        let mut btree: OwnBTreeMap<crate::String<&Counting>, u64, &Counting> = OwnBTreeMap::new_in(&alloc);
        let mut model: BTreeMap<String, u64> = BTreeMap::new();
        let (mut made, mut misses) = (0usize, 0usize);
        // Enough distinct keys to split B-tree leaves, under Miri too.
        let keys = sized(64, 24);
        for _ in 0..tape.index(sized(200, 40), "steps") {
            let key = format!("k{}", tape.below(keys, "key"));
            let value = tape.below(1000, "value");
            if !model.contains_key(&key) {
                misses += 1;
            }
            let expect = *model.entry(key.clone()).or_insert(value);
            let mut make = |k: &str| {
                made += 1;
                (crate::String::from_str_in(k, &alloc), value)
            };
            let got_sorted = *sorted.get_or_insert_with(key.as_str(), &mut make);
            let got_btree = *btree.get_or_insert_with(key.as_str(), &mut make);
            if (got_sorted, got_btree) != (expect, expect) {
                return Err(Broke::Oracle(format!("{key}: {got_sorted}, {got_btree}, model {expect}")));
            }
            if made != 2 * misses {
                return Err(Broke::Oracle(format!("{made} keys made for {misses} misses (two maps)")));
            }
        }
        same("sorted map", sorted.iter().map(|(k, v)| (String::from(k.as_str()), *v)), model.clone().into_iter())?;
        same("btree map", btree.iter().map(|(k, v)| (String::from(k.as_str()), *v)), model.into_iter())?;
    }
    alloc.check()
}

#[test]
fn get_or_insert_with_makes_a_key_only_on_a_miss() {
    blaze_sim::check(cases(200), get_or_insert_case);
}

/// `make` giving a key not equal to the one looked up panics, and leaves both maps as they were.
#[test]
fn get_or_insert_with_refuses_a_wrong_key() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut sorted: SortedMap<u64, u64, &Counting> = SortedMap::new_in(&alloc);
        let mut btree: OwnBTreeMap<u64, u64, &Counting> = OwnBTreeMap::new_in(&alloc);
        sorted.insert(1, 1);
        btree.insert(1, 1);
        panics(|| drop(sorted.get_or_insert_with(&2, |_| (3, 0))))?;
        panics(|| drop(btree.get_or_insert_with(&2, |_| (3, 0))))?;
        same("sorted map", sorted.iter().map(|(k, v)| (*k, *v)), [(1, 1)].into_iter())?;
        same("btree map", btree.iter().map(|(k, v)| (*k, *v)), [(1, 1)].into_iter())?;
    }
    alloc.check()
}

// ---------------------------------------------------------------------------------------------
// Paths.

/// A path from the tape: one to four components from a few names, so paths share directories
/// and a name is both a directory and a file somewhere.
fn a_path(tape: &Tape) -> String {
    const NAMES: [&str; 6] = ["a", "b", "src", "lib.rs", "é", "crates"];
    let depth = 1 + tape.index(4, "depth");
    let parts: StdVec<&str> = (0..depth).map(|_| NAMES[tape.index(NAMES.len(), "name")]).collect();
    parts.join("/")
}

fn written(write: impl FnOnce(&mut dyn FnMut(&str))) -> String {
    let mut out = String::new();
    write(&mut |piece| out.push_str(piece));
    out
}

fn paths_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut ours: Paths<&Counting> = Paths::new_in(&alloc);
        let mut model: BTreeMap<String, PathId> = BTreeMap::new();
        for _ in 0..tape.index(sized(80, 20), "steps") {
            let path = a_path(tape);
            if tape.below(3, "op") == 0 {
                same("find_path", ours.find_path(&path).into_iter(), model.get(&path).copied().into_iter())?;
                continue;
            }
            let id = ours.intern_path(&path);
            if let Some(&was) = model.get(&path) {
                if was != id {
                    return Err(Broke::Oracle(format!("{path:?} interned again as {id:?}, first {was:?}")));
                }
            }
            model.insert(path, id);
            if ours.len() != model.len() {
                return Err(Broke::Oracle(format!("{} files, model {}", ours.len(), model.len())));
            }
        }
        for (path, &id) in &model {
            let text = written(|put| ours.write(id, &mut |s| put(s)));
            if &text != path {
                return Err(Broke::Oracle(format!("{id:?} writes {text:?}, interned as {path:?}")));
            }
            same("name", [ours.name(id)].into_iter(), [path.rsplit('/').next().unwrap_or("")].into_iter())?;
        }
        // Sealed: every path found under its new id and written the same; the columns pass the
        // check; a walk in name order meets every path once.
        let (sealed, remap) = ours.seal_in(&alloc);
        let view = match PathsView::new(sealed.columns()) {
            Ok(view) => view,
            Err(bad) => return Err(Broke::Oracle(format!("the sealed columns fail their check: {bad}"))),
        };
        for (path, &id) in &model {
            let new = remap.path(id);
            same("sealed find_path", view.find_path(path).into_iter(), [new].into_iter())?;
            let text = written(|put| view.write(new, &mut |s| put(s)));
            if &text != path {
                return Err(Broke::Oracle(format!("sealed {new:?} writes {text:?}, not {path:?}")));
            }
        }
        let mut walked: StdVec<String> = StdVec::new();
        let mut stack = std::vec![DirId::ROOT];
        while let Some(d) = stack.pop() {
            let names: StdVec<&str> = view.files_in(d).map(|p| view.name(p)).collect();
            if names.windows(2).any(|w| w[0] >= w[1]) {
                return Err(Broke::Oracle(format!("files of {d:?} not in name order: {names:?}")));
            }
            walked.extend(view.files_in(d).map(|p| written(|put| view.write(p, &mut |s| put(s)))));
            stack.extend(view.child_dirs(d));
        }
        walked.sort();
        same("the walk", walked.iter(), model.keys())?;
        // The control (rule 53): a file moved to another directory's run is refused.
        let cols = sealed.columns();
        if view.dirs() > 1 && view.len() > 0 {
            let mut file_dir: StdVec<u32> = cols.file_dir.to_vec();
            file_dir[0] = (file_dir[0] + 1) % view.dirs() as u32;
            if PathsView::new(PathColumns { file_dir: &file_dir, ..cols }).is_ok() {
                return Err(Broke::Oracle(String::from("columns with a file in the wrong run passed the check")));
            }
        }
    }
    alloc.check()
}

#[test]
fn paths_match_a_set_of_strings_built_and_sealed() {
    blaze_sim::check(cases(300), paths_case);
}

#[test]
fn paths_refuse_an_empty_component() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut ours: Paths<&Counting> = Paths::new_in(&alloc);
        for bad in ["", "a//b", "/a", "a/"] {
            panics(|| drop(ours.intern_path(bad)))?;
        }
        if !ours.is_empty() || ours.dirs() > 2 {
            return Err(Broke::Oracle(format!("{ours:?} after only refused paths")));
        }
    }
    alloc.check()
}

// ---------------------------------------------------------------------------------------------
// Segs and SharedSegs.

fn segs_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    tokens::fresh();
    {
        let mut ours: Segs<Token, &Counting> = Segs::new_in(&alloc);
        let mut model: StdVec<u64> = StdVec::new();
        // Past the fourth segment (8 + 16 + 32 + 64 = 120 values), under Miri the second.
        let steps = tape.index(sized(300, 40), "steps");
        // A reference taken early must still read its value after every later push.
        let mut pinned: Option<(*const Token, u64)> = None;
        for _ in 0..steps {
            match tape.below(8, "op") {
                0..=5 => {
                    let v = tape.below(1000, "value");
                    let at = ours.push(Token::new(v));
                    if at != model.len() {
                        return Err(Broke::Oracle(format!("push gave {at}, model {}", model.len())));
                    }
                    model.push(v);
                    if pinned.is_none() {
                        pinned = Some((&ours[at] as *const Token, v));
                    }
                }
                6 => {
                    let i = tape.index(model.len() + 1, "index");
                    same("get", ours.get(i).map(|t| t.value).into_iter(), model.get(i).copied().into_iter())?;
                }
                _ if tape.below(4, "clear") == 0 => {
                    ours.clear();
                    model.clear();
                    pinned = None;
                }
                _ => {}
            }
            if let Some((at, v)) = pinned {
                // SAFETY: the vector never moves a value; it is still live (no clear since).
                let seen = unsafe { (*at).value };
                if seen != v {
                    return Err(Broke::Oracle(format!("a value moved: {seen} where {v} was")));
                }
            }
        }
        if ours.len() != model.len() {
            return Err(Broke::Oracle(format!("{} values, model {}", ours.len(), model.len())));
        }
        same("the values", ours.iter().map(|t| t.value), model.iter().copied())?;
        same("by index", (0..model.len()).map(|i| ours[i].value), model.iter().copied())?;
        same(
            "segment lengths",
            ours.segments().map(<[Token]>::len),
            (0..).map(|s| 8usize << s).scan(model.len(), |left, n| {
                (*left > 0).then(|| {
                    let here = n.min(*left);
                    *left -= here;
                    here
                })
            }),
        )?;
        tokens::check(model.len())?;
    }
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn segs_match_a_vec_and_never_move() {
    blaze_sim::check(cases(200), segs_case);
}

#[test]
fn segs_try_push_fails_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("Segs", |alloc| {
        let mut ours: Segs<u64, _> = Segs::new_in(alloc);
        for v in 0..sized(100u64, 30) {
            step(&mut ours, |s| s.iter().copied().collect::<StdVec<_>>(), |s| s.try_push(v))?;
        }
        Ok(())
    })
    .map(drop)
}

/// The shared form on real threads (its interleavings are the model's, under `cfg(blaze_check)`):
/// one appender, readers that see a prefix that only grows and every value in it whole.
#[test]
fn shared_segs_readers_see_a_growing_prefix() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let total = sized(20_000usize, 40);
        let segs: SharedSegs<u64, &Counting> = SharedSegs::new_in(&alloc);
        let wrong: Mutex<Option<String>> = Mutex::new(None);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    let mut seen = 0;
                    while seen < total {
                        let len = segs.len();
                        if len < seen {
                            *wrong.lock().unwrap_or_else(PoisonError::into_inner) = Some(format!("the length fell from {seen} to {len}"));
                            return;
                        }
                        for i in seen..len {
                            if segs.get(i).copied() != Some(i as u64 * 3) {
                                *wrong.lock().unwrap_or_else(PoisonError::into_inner) = Some(format!("index {i} read wrong"));
                                return;
                            }
                        }
                        seen = len;
                        std::thread::yield_now();
                    }
                });
            }
            let mut appender = segs.appender().expect("the first appender");
            if segs.appender().is_some() {
                *wrong.lock().unwrap_or_else(PoisonError::into_inner) = Some(String::from("a second appender while one is out"));
            }
            for i in 0..total {
                appender.push(i as u64 * 3);
            }
        });
        if let Some(wrong) = wrong.into_inner().unwrap_or_else(PoisonError::into_inner) {
            return Err(Broke::Oracle(wrong));
        }
        if segs.appender().is_none() {
            return Err(Broke::Oracle(String::from("no appender after the first dropped")));
        }
    }
    alloc.check()
}
