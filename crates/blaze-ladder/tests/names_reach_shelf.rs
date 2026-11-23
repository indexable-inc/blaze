//! How much of each dependency a real program's NAMES reach (engine.md piece E13, D134: lazy
//! names of dependencies; rule 16, measured before it is built): a `std` program's graph on the
//! real toolchain shelf, solved once with every crate's groups asked for
//! (tests/names_groups_shelf.rs), then, from the group records ALONE, the closure of what the
//! root crate reads: every group of the root; each read (`GroupRead::Binding`, `Globs`,
//! `Textual`: the module it names) followed to the group holding that module, in its crate or a
//! dependency's, transitively. That closure is what a lazy names pass at GROUP grain must solve.
//!
//! At CELL grain the records say less: a read names one cell (`table`, `name`), but a group's
//! record holds the reads of the group as a whole, not of each cell, and never the reads inside
//! it. So the cells counted (distinct cells the closure's groups read in a crate, and those the
//! root reads directly) against the cells the crate's groups define are a LOWER bound of what a
//! cell-grain pass decides: deciding a cell may read others of its own group.
//!
//! What the names do not show: paths in fn signatures and bodies are the front end's, not the
//! names solve's (the root's text below names what its bodies use by `use` lines, so the names
//! read them), and trait questions read impl tables (tests/front2_tables_core.rs).
//!
//! Prints, per crate (the 24 with the most modules, the rest summed):
//! `REACH <crate>: <m> modules, <g> groups; reached <rg> groups, <rm> modules (<p>%), <rb>/<b>
//! record bytes; cells read <c> (root directly <d>) of <n> defined; <u> reads of no group`
//! then `DEPS` (the dependencies' totals) and `SOLVE` (seconds).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as
//! tests/names_groups_shelf.rs.

#![feature(allocator_api)]

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::Instant;

use blaze_id::ItemKey;
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_own::AVec;
use blaze_scope::GroupRead;
use blaze_scope::task::{Par, Serial};

/// The serial solve, with every crate's groups asked for.
struct Grouped;

impl Par for Grouped {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        Serial.map(n, f, out);
    }

    fn names_groups(&self) -> bool {
        true
    }
}

const HELLO: &str = "fn main() {
    println!(\"hello\");
}
";

/// A root that uses more of `std`: collections, `String`, `fmt`, an iterator chain, `io::Write`,
/// each named by a `use` line too (the names solve reads imports and macro calls, not bodies).
const MORE: &str = "use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::io::Write;
use std::iter;
use std::string::String;
use std::vec::Vec;

struct P(u32);

impl fmt::Display for P {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, \"{}\", self.0)
    }
}

fn main() {
    let mut m: HashMap<String, Vec<u32>> = HashMap::new();
    m.entry(String::from(\"a\")).or_default().push(1);
    let v: Vec<u32> = (0..10).filter(|x| x % 2 == 0).map(|x| x * 3).collect();
    let mut s = String::new();
    write!(s, \"{:?} {}\", v, P(3)).unwrap();
    let total: u32 = iter::once(1).chain(v.iter().copied()).sum();
    writeln!(std::io::stdout(), \"{s} {total} {}\", m.len()).unwrap();
    println!(\"{}\", format!(\"{total}\"));
}
";

/// One crate's reach.
#[derive(Default, Clone)]
struct Row {
    name: String,
    modules: usize,
    groups: usize,
    reached_groups: usize,
    reached_modules: usize,
    bytes: usize,
    reached_bytes: usize,
    cells_read: usize,
    root_cells: usize,
    defined: usize,
    unmapped: usize,
    /// Groups solved on demand (E13), of them outside the predicted closure, and walks.
    on_demand: usize,
    outside: usize,
    walks: usize,
}

impl Row {
    fn add(&mut self, o: &Row) {
        self.on_demand += o.on_demand;
        self.outside += o.outside;
        self.walks += o.walks;
        self.modules += o.modules;
        self.groups += o.groups;
        self.reached_groups += o.reached_groups;
        self.reached_modules += o.reached_modules;
        self.bytes += o.bytes;
        self.reached_bytes += o.reached_bytes;
        self.cells_read += o.cells_read;
        self.root_cells += o.root_cells;
        self.defined += o.defined;
        self.unmapped += o.unmapped;
    }

    fn line(&self, tag: &str) -> String {
        let pct = |a: usize, b: usize| if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 };
        format!(
            "{tag} {}: {} modules, {} groups; reached {} groups, {} modules ({:.1}%), {}/{} record bytes ({:.1}%); cells read {} (root directly {}) of {} defined ({:.1}%); {} reads of no group; ON DEMAND {} groups ({} outside the reach), {} walks",
            self.name,
            self.modules,
            self.groups,
            self.reached_groups,
            self.reached_modules,
            pct(self.reached_modules, self.modules),
            self.reached_bytes,
            self.bytes,
            pct(self.reached_bytes, self.bytes),
            self.cells_read,
            self.root_cells,
            self.defined,
            pct(self.cells_read, self.defined),
            self.unmapped,
            self.on_demand,
            self.outside,
            self.walks,
        )
    }
}

/// What a table no group holds is, in its crate's names: an enum (its variants' pseudo-module),
/// a module the walk found or a macro made but no group wrote, a module that failed, an item
/// with text (a trait, a struct: an associated-name table), or none of those.
fn table_kind(c: &blaze_scope::resolve::CrateNames<'_>, table: ItemKey) -> &'static str {
    if c.tables.enums.contains_key(&table) {
        "enum"
    } else if c.failed.contains_key(&table) {
        "module whose task failed"
    } else if c.tree.by_key.contains_key(&table) {
        if c.done.contains_key(&table) { "walked module, done, in no group" } else { "walked module, not done" }
    } else if c.made.contains_key(&table) {
        if c.done.contains_key(&table) { "made module, done, in no group" } else { "made module, not done" }
    } else if c.tables.members.contains_key(&table) {
        "trait or impl"
    } else if c.tables.texts.contains_key(&table) {
        "item with text (not a module)"
    } else {
        "key of nothing the crate's names hold"
    }
}

/// Solves `main`'s graph once with groups and prints the closure of what its root reads; the
/// lines are also written to `<temp>/names_reach_shelf.<label>.out`.
fn reach(label: &str, main: &str) {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/reach/src/main.rs".to_string(), main.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "reach", dir: "/reach/", root: "/reach/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let t0 = Instant::now();
    let names = inputs.resolve_with(&crates, &Grouped, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates with groups: {e}"));
    let solve_s = t0.elapsed().as_secs_f64();

    let all = names.crates();
    let root = all.iter().position(|c| c.name == "reach").expect("the root crate is in the graph");
    // Each written module's group.
    let mut at: HashMap<ItemKey, (usize, usize)> = HashMap::new();
    for (k, c) in all.iter().enumerate() {
        for (gi, grp) in c.groups.iter().enumerate() {
            for m in grp.members.iter() {
                at.insert(*m, (k, gi));
            }
        }
    }
    // The closure, breadth first from every group of the root.
    let mut reached: Vec<Vec<bool>> = all.iter().map(|c| vec![false; c.groups.len()]).collect();
    let mut todo: VecDeque<(usize, usize)> = VecDeque::new();
    for gi in 0..all[root].groups.len() {
        reached[root][gi] = true;
        todo.push_back((root, gi));
    }
    let mut cells: Vec<BTreeSet<(ItemKey, [u8; 16])>> = vec![BTreeSet::new(); all.len()];
    let mut root_cells: Vec<BTreeSet<(ItemKey, [u8; 16])>> = vec![BTreeSet::new(); all.len()];
    let mut unmapped = vec![0usize; all.len()];
    // The reads of a table no group holds, by (crate, read kind, table kind): count, an example.
    let mut unmapped_kinds: std::collections::BTreeMap<(String, &str, &str), (usize, String)> = std::collections::BTreeMap::new();
    while let Some((k, gi)) = todo.pop_front() {
        for r in all[k].groups[gi].reads.iter() {
            let target = match *r {
                GroupRead::Binding { table, .. } | GroupRead::Globs { table, .. } => table,
                GroupRead::Textual { module, .. } => module,
                // A member's own call: inside the group.
                GroupRead::Expansion { .. } => continue,
            };
            let tk = names.crate_of(target);
            // A read across the crate line is a lookup's whole answer (blaze-scope
            // `Built::lookup_in`): it walks the table's glob edges where the name is not bound
            // there, and the reader's record names the table alone. So the glob sources,
            // transitively, are reached too (an upper bound: a name bound in the table walks
            // none). The fixture tests/names_groups_lazy_deps.rs met it first (`b` -> `m11::*`).
            if tk.is_some_and(|tk| tk != k) {
                let mut walk = vec![target];
                let mut seen: BTreeSet<ItemKey> = BTreeSet::new();
                while let Some(t) = walk.pop() {
                    if !seen.insert(t) {
                        continue;
                    }
                    if let Some(&(sk, sg)) = at.get(&t)
                        && !reached[sk][sg]
                    {
                        reached[sk][sg] = true;
                        todo.push_back((sk, sg));
                    }
                    if let Some(sk) = names.crate_of(t)
                        && let Some(r) = all[sk].done.get(&t)
                    {
                        walk.extend(r.scope.globs.iter().map(|e| e.source.key));
                    }
                }
            }
            if let (Ok((t, cell, _)), Some(tk)) = (r.engine(), tk) {
                cells[tk].insert((t, cell));
                if k == root {
                    root_cells[tk].insert((t, cell));
                }
            }
            match at.get(&target) {
                Some(&(tk, tg)) => {
                    if !reached[tk][tg] {
                        reached[tk][tg] = true;
                        todo.push_back((tk, tg));
                    }
                }
                None => {
                    if let Some(tk) = tk {
                        unmapped[tk] += 1;
                        let read = match *r {
                            GroupRead::Binding { .. } => "binding",
                            GroupRead::Globs { .. } => "globs",
                            GroupRead::Textual { .. } => "textual",
                            GroupRead::Expansion { .. } => "expansion",
                        };
                        let kind = table_kind(all[tk], target);
                        let e = unmapped_kinds.entry((all[tk].name.to_string(), read, kind)).or_insert((0, String::new()));
                        e.0 += 1;
                        if e.1.is_empty() {
                            e.1 = all[tk].path_of(target).map_or_else(|| format!("{target:?}"), str::to_string);
                        }
                    }
                }
            }
        }
    }

    // E13: the same graph with the root solved whole and every dependency on demand
    // (blaze-scope cells/lazy_deps.rs): the groups each crate ACTUALLY solved.
    let t1 = Instant::now();
    // A refusal is printed, not hidden: the column then stays empty and its checks are not run
    // (2026-11-17: core's `MemberRefusal::Textual`, a `macro_rules!` walk out of the members).
    let (lazy, refused) = match inputs.names_on_demand(&crates, &[root], blaze_own::testing::bump()) {
        Ok(l) => (Some(l), None),
        Err(e) => (None, Some(format!("{e:?}"))),
    };
    let lazy_s = t1.elapsed().as_secs_f64();
    let mut mismatched: Vec<String> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    for (k, c) in all.iter().enumerate() {
        let mut row = Row { name: c.name.to_string(), modules: c.done.len(), groups: c.groups.len(), ..Row::default() };
        if k != root
            && let Some(lazy) = lazy.as_ref()
        {
            let got = &lazy.demanded[k];
            row.on_demand = got.groups.len();
            row.walks = lazy.walks[k];
            for grp in got.groups.iter() {
                match at.get(&grp.members[0]) {
                    Some(&(wk, wg)) if wk == k && all[k].groups[wg].members[..] == grp.members[..] && all[k].groups[wg].bytes[..] == grp.bytes[..] => {
                        if !reached[k][wg] {
                            row.outside += 1;
                        }
                    }
                    _ => mismatched.push(format!("{}: {:?}", c.name, c.path_of(grp.members[0]))),
                }
            }
        }
        for (gi, grp) in c.groups.iter().enumerate() {
            row.bytes += grp.bytes.len();
            row.defined += grp.defines.len();
            if reached[k][gi] {
                row.reached_groups += 1;
                row.reached_modules += grp.members.len();
                row.reached_bytes += grp.bytes.len();
            }
        }
        row.cells_read = cells[k].len();
        row.root_cells = root_cells[k].len();
        row.unmapped = unmapped[k];
        rows.push(row);
    }

    let mut said = String::new();
    let mut say = |line: String| {
        println!("{line}");
        said.push_str(&line);
        said.push('\n');
    };
    // First, so a 30-line window of the output holds it.
    let (want, got, out): (usize, usize, usize) = rows.iter().enumerate().filter(|(k, _)| *k != root).fold((0, 0, 0), |a, (_, r)| (a.0 + r.reached_groups, a.1 + r.on_demand, a.2 + r.outside));
    match (&lazy, &refused) {
        (Some(lazy), _) => {
            // B2's floor: one walk per crate read (its tree record taken by every one-group
            // solve after it), none for a crate never read.
            let deps = || (0..all.len()).filter(|&k| k != root);
            let walks: usize = deps().map(|k| lazy.walks[k]).sum();
            let walked = deps().filter(|&k| lazy.walks[k] > 0).count();
            let most = deps().max_by_key(|&k| (lazy.walks[k], core::cmp::Reverse(k)));
            let most = most.map_or_else(String::new, |k| format!("{} {}", all[k].name, lazy.walks[k]));
            let one_group: usize = deps().map(|k| lazy.one_group_solves[k]).sum();
            say(format!(
                "ONDEMAND {label}: {got} groups solved on demand against {want} predicted by the reach, {out} of them outside it; walks {walks} over {walked} crates walked (most: {most}; floor 1 a crate); {one_group} one-group solves; {} root solves; {:.2} s (the whole solve {solve_s:.2} s); {} groups not byte-equal; by crate: {}",
                lazy.root_solves,
                lazy_s,
                mismatched.len(),
                lazy.counts
            ))
        }
        (None, refused) => say(format!("ONDEMAND {label}: REFUSED after {lazy_s:.2} s (predicted reach {want} groups): {}", refused.as_deref().unwrap_or("?"))),
    }
    say(format!("ROOT {label}: {}", rows[root].line("crate")));
    let mut deps: Vec<Row> = rows.iter().enumerate().filter(|(k, _)| *k != root).map(|(_, r)| r.clone()).collect();
    deps.sort_by(|a, b| b.modules.cmp(&a.modules).then_with(|| a.name.cmp(&b.name)));
    let mut rest = Row { name: format!("the other {} crates", deps.len().saturating_sub(24)), ..Row::default() };
    let mut total = Row { name: format!("{} dependencies", deps.len()), ..Row::default() };
    for (i, d) in deps.iter().enumerate() {
        if i < 24 {
            say(d.line("REACH"));
        } else {
            rest.add(d);
        }
        total.add(d);
    }
    if deps.len() > 24 {
        say(rest.line("REACH"));
    }
    say(total.line("DEPS"));
    for ((krate, read, kind), (n, example)) in unmapped_kinds.iter() {
        say(format!("NOGROUP {label} {krate}: {n} {read} reads of a {kind} (e.g. {example})"));
    }
    say(format!("SOLVE {label}: {solve_s:.2} s for {} crates with groups", all.len()));
    let out = std::env::temp_dir().join(format!("names_reach_shelf.{label}.out"));
    std::fs::write(&out, said).unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));

    assert!(rows[root].reached_groups == rows[root].groups, "the root's own groups are all in its closure");
    assert!(mismatched.is_empty(), "groups solved on demand that are not byte-equal to the whole solve's: {mismatched:?}");
    if let Some(lazy) = lazy.as_ref() {
        let root_lazy = lazy.roots[root].expect("the root was solved");
        assert!(
            root_lazy.groups.len() == all[root].groups.len() && root_lazy.groups.iter().zip(all[root].groups.iter()).all(|(a, b)| a.bytes[..] == b.bytes[..]),
            "the root's groups on demand differ from the whole solve's"
        );
    }
    let std_row = rows.iter().find(|r| r.name == "std").expect("std is in the graph");
    assert!(std_row.reached_groups > 0, "`{label}`'s root reads nothing of std: the reads are not recorded");
}

// One solve of the shelf each, about a minute in a test build: asked by name
// (`-p blaze-ladder --test names_reach_shelf -- --ignored --show-output`).
#[test]
#[ignore = "one solve of the shelf; asked by name"]
fn a_hello_world_s_names_reach() {
    reach("hello", HELLO);
}

#[test]
#[ignore = "one solve of the shelf; asked by name"]
fn a_program_using_more_of_std_s_names_reach() {
    reach("more", MORE);
}
