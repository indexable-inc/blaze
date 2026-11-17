//! Lints every `.rs` file under a directory item by item, as a driver will: each file outlined,
//! the root linted with its items as holes, each item in the scope its parent's pass gave it.
//! Prints the files, items and findings per rule and kind, the fixes there are, the time on one
//! thread and on every core beside one lex of the same bytes (the floor), and the time to lint
//! again after one item changes (only the units whose key changed).
//!
//! `cargo run --release -p blaze-lint --example run -- crates`

#![feature(allocator_api)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use blaze_file_items::outline;
use blaze_lint::{Findings, Rule, RuleSet, Scope, Span, TestBits, Unit, Variants, lex_floor, lint_in};
use blaze_own::Alloc;
use blaze_surface::{Edition, ItemDetail, ItemIndex};

/// One unit of a file: its range, its children's ranges (item-relative), which units they are.
struct UnitPlan {
    base: u32,
    end: u32,
    holes: Vec<Span>,
    children: Vec<usize>,
}

struct File {
    path: PathBuf,
    parts: Vec<String>,
    text: String,
    units: Vec<UnitPlan>,
    /// The out-of-line `mod name;` declarations: unit index and name.
    mods: Vec<(usize, String)>,
    bin_root: bool,
    build_script: bool,
    test: bool,
}

const TEST_DIRS: [&str; 2] = ["tests", "benches"];
const TEST_FILES: [&str; 2] = ["tests.rs", "test.rs"];
const BIN_DIRS: [&str; 2] = ["bin", "examples"];
const ROOT_FILES: [&str; 4] = ["lib.rs", "main.rs", "mod.rs", "build.rs"];

fn plan(path: PathBuf, parts: Vec<String>, text: String, heap: &'static blaze_own::Heap) -> File {
    let alloc = heap.alloc();
    let lines = outline(&text, Edition::E2024, alloc);
    let items = lines.items();
    let mut units = Vec::with_capacity(items.len() + 1);
    let mut mods = Vec::new();
    // Unit 0 is the root; unit `i + 1` is entry `i`.
    for at in 0..=items.len() {
        let (base, end, parent) = match at {
            0 => (0, text.len() as u32, None),
            _ => {
                let item = &items[at - 1];
                if let ItemDetail::Mod { inline: false, .. } = item.detail
                    && let Some(name) = item.name
                {
                    let name = &text[(item.range.start + name.start) as usize..(item.range.start + name.end) as usize];
                    mods.push((at, name.trim_start_matches("r#").to_string()));
                }
                (item.range.start, item.range.end, Some(ItemIndex(at as u32 - 1)))
            }
        };
        let mut holes = Vec::new();
        let mut children = Vec::new();
        for (index, child) in lines.children(parent) {
            holes.push(Span::new(child.range.start - base, child.range.end - base));
            children.push(index.at() + 1);
        }
        units.push(UnitPlan { base, end, holes, children });
    }
    let name = parts.last().map_or("", String::as_str);
    let dirs = &parts[..parts.len().saturating_sub(1)];
    let test = dirs.iter().any(|d| TEST_DIRS.contains(&d.as_str())) || TEST_FILES.contains(&name);
    let build_script = name == "build.rs" && dirs.last().is_some_and(|d| d != "src");
    let bin_root = match parts.as_slice() {
        [.., up, _, name] if name == "main.rs" && BIN_DIRS.contains(&up.as_str()) => true,
        [.., dir, name] if name == "main.rs" && dir == "src" => true,
        [.., dir, _] => BIN_DIRS.contains(&dir.as_str()),
        _ => false,
    };
    File { path, parts, text, units, mods, bin_root, build_script, test }
}

impl File {
    fn scope(&self) -> Scope {
        Scope::file(if self.test { TestBits::CFG } else { TestBits::NONE }, self.bin_root, self.build_script)
    }

    /// Lints every unit in order; `each` sees each unit's index, scope and the findings it
    /// pushed. Returns the units' scopes.
    fn lint_all<'a>(
        &self,
        rules: &RuleSet,
        out: &mut Findings<'a>,
        alloc: Alloc<'a>,
        mut each: impl FnMut(usize, Scope, &Findings<'a>, blaze_lint::Run, blaze_lint::TestBits),
    ) -> Vec<Scope> {
        let mut scopes = vec![self.scope(); self.units.len()];
        // `variant-count`'s fact of the file, gathered once (one lex where the file has `enum`).
        let variants = match rules.runs(Rule::VariantCount) {
            true => Variants::gather(&self.text, alloc).unwrap_or_else(|error| panic!("{}: {error:?}", self.path.display())),
            false => Variants::new_in(alloc),
        };
        for (at, unit) in self.units.iter().enumerate() {
            let input = Unit { text: &self.text[unit.base as usize..unit.end as usize], holes: &unit.holes, scope: scopes[at] };
            let linted = match lint_in(rules, &input, &variants, unit.base, alloc, out) {
                Ok(linted) => linted,
                Err(error) => panic!("{}: unit {at}: {error:?}", self.path.display()),
            };
            for (child, scope) in unit.children.iter().zip(linted.children.iter()) {
                scopes[*child] = *scope;
            }
            each(at, scopes[at], out, linted.findings, linted.test_at_end);
        }
        scopes
    }
}

fn module_files(parts: &[String], name: &str) -> [Vec<String>; 2] {
    let (file, dirs) = parts.split_last().map_or(("", &[][..]), |(f, d)| (f.as_str(), d));
    let mut base: Vec<String> = dirs.to_vec();
    if !(ROOT_FILES.contains(&file) || dirs.last().is_some_and(|d| d == "bin")) {
        base.push(file.trim_end_matches(".rs").to_string());
    }
    let mut one = base.clone();
    one.push(format!("{name}.rs"));
    let mut two = base;
    two.push(name.to_string());
    two.push("mod.rs".to_string());
    [one, two]
}

fn walk(root: &Path) -> Vec<(PathBuf, Vec<String>)> {
    let mut out = Vec::new();
    let start: Vec<String> = root.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let mut todo = vec![(root.to_path_buf(), start)];
    // Bounded: each directory is listed once; links are not followed.
    while let Some((dir, parts)) = todo.pop() {
        let Ok(listed) = std::fs::read_dir(&dir) else { continue };
        for entry in listed.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let mut sub = parts.clone();
            sub.push(name.clone());
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && name != "target" {
                todo.push((entry.path(), sub));
            } else if !is_dir && name.ends_with(".rs") {
                out.push((entry.path(), sub));
            }
        }
    }
    out.sort();
    out
}

/// Runs `work` over every file on `threads` threads taking files from one counter; best of 3.
fn timed(files: &[File], threads: usize, work: &(dyn Fn(&File) -> usize + Sync)) -> (f64, usize) {
    let mut fastest = f64::MAX;
    let mut total = 0;
    for _ in 0..3 {
        let next = AtomicUsize::new(0);
        let counted = AtomicUsize::new(0);
        let start = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    let mut mine = 0;
                    while let Some(file) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                        mine += work(file);
                    }
                    counted.fetch_add(mine, Ordering::Relaxed);
                });
            }
        });
        fastest = fastest.min(start.elapsed().as_secs_f64());
        total = counted.load(Ordering::Relaxed);
    }
    (fastest, total)
}

fn lint_file(file: &File, heap: &'static blaze_own::Heap) -> usize {
    let alloc = heap.alloc();
    let mut out = Findings::new_in(alloc);
    file.lint_all(&RuleSet::ALL, &mut out, alloc, |_, _, _, _, _| {});
    out.list.len()
}

fn ms(seconds: f64) -> String {
    format!("{:.1} ms", seconds * 1e3)
}

fn main() {
    let heap = blaze_own::Heap::process();
    let root = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "crates".to_string()));
    let mut files: Vec<File> = walk(&root)
        .into_iter()
        .filter_map(|(path, parts)| {
            let bytes = std::fs::read(&path).ok()?;
            let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
            Some(plan(path, parts, text, heap))
        })
        .collect();

    // Which files are test code: a test path, or a module declared in test code (to a fixed
    // point, as the scripts' worklist does). No rule runs: only the test regions are read.
    let index: HashMap<Vec<String>, usize> = files.iter().enumerate().map(|(i, f)| (f.parts.clone(), i)).collect();
    let none = RuleSet::only(&[]);
    for _round in 0..16 {
        let mut marked = HashSet::new();
        for file in &files {
            let alloc = heap.alloc();
            let mut out = Findings::new_in(alloc);
            let mut test_mods = HashSet::new();
            file.lint_all(&none, &mut out, alloc, |at, scope, _, _, at_end| {
                if file.test || scope.test != TestBits::NONE || at_end != TestBits::NONE {
                    test_mods.insert(at);
                }
            });
            for (at, name) in &file.mods {
                if test_mods.contains(at) {
                    for candidate in module_files(&file.parts, name) {
                        if let Some(&i) = index.get(&candidate)
                            && !files[i].test
                        {
                            marked.insert(i);
                        }
                    }
                }
            }
        }
        if marked.is_empty() {
            break;
        }
        for i in marked {
            files[i].test = true;
        }
    }

    // The findings, counted per rule and kind; the per-line counts are what the scripts count.
    // With a second argument, every finding is also written there as `rule kind file line` (the
    // line 1-based, as the scripts give it), to compare with them line by line.
    let mut dump = std::env::args().nth(2).map(|_| String::new());
    let mut per_kind: BTreeMap<(u16, &'static str, &'static str), (usize, HashSet<(usize, usize)>)> = BTreeMap::new();
    let (mut units, mut fixes, mut bytes) = (0usize, 0usize, 0usize);
    for (number, file) in files.iter().enumerate() {
        bytes += file.text.len();
        units += file.units.len();
        let alloc = heap.alloc();
        let mut out = Findings::new_in(alloc);
        file.lint_all(&RuleSet::ALL, &mut out, alloc, |at, _, out, run, _| {
            let base = file.units[at].base as usize;
            for finding in &out.list[run.from as usize..run.to as usize] {
                let line = file.text[..base + finding.range.start as usize].matches('\n').count();
                let rule = finding.rule();
                let row = per_kind.entry((rule.id(), rule.name(), finding.template.kind())).or_default();
                row.0 += 1;
                row.1.insert((number, line));
                fixes += out.fixes_of(finding).len();
                if let Some(dump) = &mut dump {
                    dump.push_str(&format!("{} {} {} {}\n", rule.name(), finding.template.kind(), file.parts.join("/"), line + 1));
                }
            }
        });
    }
    if let (Some(dump), Some(to)) = (&dump, std::env::args().nth(2))
        && let Err(error) = std::fs::write(&to, dump)
    {
        panic!("writing {to}: {error}");
    }
    println!("files {}  items (units) {}  bytes {}", files.len(), units, bytes);
    println!("findings per rule and kind (all, and distinct lines):");
    let mut per_rule: BTreeMap<&str, usize> = BTreeMap::new();
    for ((_, rule, kind), (all, lines)) in &per_kind {
        println!("  {rule:<15} {kind:<18} {all:>6} {:>6}", lines.len());
        *per_rule.entry(rule).or_default() += all;
    }
    for rule in Rule::ALL {
        println!("  {:<15} total {:>6}", rule.name(), per_rule.get(rule.name()).copied().unwrap_or(0));
    }
    println!("fixes available {fixes}");

    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let (lex_one, tokens) = timed(&files, 1, &|f: &File| lex_floor(&f.text));
    let (lint_one, found) = timed(&files, 1, &|f: &File| lint_file(f, heap));
    let (lex_all, _) = timed(&files, cores, &|f: &File| lex_floor(&f.text));
    let (lint_all, _) = timed(&files, cores, &|f: &File| lint_file(f, heap));
    // The pass with no rule: the lex of every unit's own runs, the block and test tracking, the
    // children's scopes. What the rules add is the rest.
    let (bare_one, _) = timed(&files, 1, &|f: &File| {
        let alloc = heap.alloc();
        let mut out = Findings::new_in(alloc);
        f.lint_all(&none, &mut out, alloc, |_, _, _, _, _| {});
        0
    });
    println!("1 thread, no rule enabled: {}", ms(bare_one));
    println!("tokens {tokens}  findings {found}");
    println!(
        "1 thread:  lint {}  lex (floor) {}  ratio {:.2}",
        ms(lint_one),
        ms(lex_one),
        lint_one / lex_one
    );
    println!(
        "{cores} threads: lint {}  lex (floor) {}  ratio {:.2}",
        ms(lint_all),
        ms(lex_all),
        lint_all / lex_all
    );

    // One edit: a statement added at the end of a leaf item in the middle of the largest file.
    // The file is outlined again; a unit is linted again only when its key (its bytes' hash and
    // its scope's key) is not one the first run kept.
    // (Vendored generated bindings are left out: one item per line, not the code people edit.)
    let Some(largest) = files.iter().filter(|f| !f.parts.iter().any(|p| p == "vendor")).max_by_key(|f| f.text.len()) else {
        return;
    };
    let alloc = heap.alloc();
    let before = outline(&largest.text, Edition::E2024, alloc);
    let mut kept: HashSet<(_, u64)> = HashSet::new();
    let mut out = Findings::new_in(alloc);
    let scopes = largest.lint_all(&RuleSet::ALL, &mut out, alloc, |_, _, _, _, _| {});
    for (at, item) in before.items().iter().enumerate() {
        kept.insert((item.hash, scopes[at + 1].key()));
    }
    let leaves: Vec<usize> = (1..largest.units.len())
        .filter(|u| largest.units[*u].children.is_empty() && largest.text[..largest.units[*u].end as usize].ends_with('}'))
        .collect();
    let Some(&target) = leaves.get(leaves.len() / 2) else { return };
    let at = largest.units[target].end as usize - 1;
    let edited = format!("{}    let edit = x.clone();\n{}", &largest.text[..at], &largest.text[at..]);
    let start = Instant::now();
    let after = plan(largest.path.clone(), largest.parts.clone(), edited, heap);
    let outlined = start.elapsed().as_secs_f64();
    let new_outline = outline(&after.text, Edition::E2024, alloc);
    let hashes: Vec<_> = new_outline.items().iter().map(|item| item.hash).collect();
    let mut out = Findings::new_in(alloc);
    // The edit adds no item, so unit `u` is the same entry before and after: a hit's children
    // start from the scopes the first run gave them, and a linted parent overwrites them.
    let mut scopes = scopes;
    let mut relinted = 0;
    let start = Instant::now();
    // The file's `variant-count` fact is gathered again: an edit's cost includes it.
    let variants = Variants::gather(&after.text, alloc).unwrap_or_else(|error| panic!("{error:?}"));
    for (u, unit) in after.units.iter().enumerate() {
        if u > 0 && kept.contains(&(hashes[u - 1], scopes[u].key())) {
            // A hit: its findings and its children's scopes are the record's.
            continue;
        }
        relinted += 1;
        let input = Unit { text: &after.text[unit.base as usize..unit.end as usize], holes: &unit.holes, scope: scopes[u] };
        match lint_in(&RuleSet::ALL, &input, &variants, unit.base, alloc, &mut out) {
            Ok(linted) => {
                for (child, scope) in unit.children.iter().zip(linted.children.iter()) {
                    scopes[*child] = *scope;
                }
            }
            Err(error) => panic!("{error:?}"),
        }
    }
    let relint = start.elapsed().as_secs_f64();
    println!(
        "one edit in {} ({} units): outline again {}, lint {} of {} units again in {:.1} us, findings now {}",
        largest.path.display(),
        largest.units.len(),
        ms(outlined),
        relinted,
        after.units.len(),
        relint * 1e6,
        out.list.len()
    );
}
