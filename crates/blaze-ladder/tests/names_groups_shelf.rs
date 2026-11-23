//! Names per group on the real shelf (blaze-scope cells/groups.rs, engine piece E4): a plain
//! `std` program's graph (core, alloc, std, libc and the rest of the toolchain's units), solved
//! with every crate's groups asked for, twice.
//!
//! The claims: no crate's groups fail (the pass meets every node the finished solve decided,
//! libc's cfg'd globs and `macro_rules!`-written items too); two solves of one text write equal
//! group records (rule 25), and libc and core each have groups. It prints, per crate:
//! `GROUPS <crate>: <modules> modules, <groups> groups, largest <n> members (<path>), <bytes>
//! bytes (<per> per group), <reads> reads`, then the solve's seconds with and without the pass.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as
//! tests/front2_tables_core.rs.

#![feature(allocator_api)]

use std::time::Instant;

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_own::AVec;
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

const MAIN: &str = "fn main() {
    println!(\"hello\");
}
";

// Three solves of the shelf, about three minutes: asked by name
// (`-p blaze-ladder --test names_groups_shelf -- --ignored`), its lines in the temp
// directory's `names_groups_shelf.out`.
#[test]
#[ignore = "three solves of the shelf; asked by name"]
fn the_shelf_s_crates_are_written_per_group() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "hello", dir: "/hello/", root: "/hello/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();

    let t0 = Instant::now();
    let plain = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let plain_s = t0.elapsed().as_secs_f64();
    drop(plain);
    let t1 = Instant::now();
    let first = inputs.resolve_with(&crates, &Grouped, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates with groups: {e}"));
    let grouped_s = t1.elapsed().as_secs_f64();
    let second = inputs.resolve_with(&crates, &Grouped, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates with groups, again: {e}"));

    // Each line printed is kept too, and written to `names_groups_shelf.out` in the temp
    // directory: the build queue shows a passing test's output to no one.
    let mut said = String::new();
    macro_rules! say {
        ($($t:tt)*) => {{
            let line = format!($($t)*);
            println!("{line}");
            said.push_str(&line);
            said.push('\n');
        }};
    }
    let mut seen = Vec::new();
    for (a, b) in first.crates().iter().zip(second.crates().iter()) {
        assert_eq!(a.name, b.name);
        let bytes: usize = a.groups.iter().map(|grp| grp.bytes.len()).sum();
        let reads: usize = a.groups.iter().map(|grp| grp.reads.len()).sum();
        let members: usize = a.groups.iter().map(|grp| grp.members.len()).sum();
        let largest = a.groups.iter().max_by_key(|grp| grp.members.len());
        let (n, path) = largest.map_or((0, String::new()), |grp| (grp.members.len(), a.path_of(grp.members[0]).map_or_else(|| format!("{:?}", grp.members[0]), ToString::to_string)));
        let per = if a.groups.is_empty() { 0 } else { bytes / a.groups.len() };
        say!("GROUPS {}: {} modules, {} groups ({members} members), largest {n} members ({path}), {bytes} bytes ({per} per group), {reads} reads", a.name, a.done.len(), a.groups.len());
        assert_eq!(a.groups.len(), b.groups.len(), "`{}`: two solves cut two ways", a.name);
        for (x, y) in a.groups.iter().zip(b.groups.iter()) {
            assert!(x.members == y.members && x.bytes == y.bytes, "`{}`: two solves of one text wrote other group records (rule 25)", a.name);
        }
        assert_eq!(members, a.done.len(), "`{}`: the groups do not cover its written modules", a.name);
        seen.push(a.name);
    }
    for name in ["core", "libc"] {
        assert!(seen.contains(&name), "no crate `{name}` in the graph: {seen:?}");
    }
    say!("GROUPS SOLVE: {plain_s:.2} s without the pass, {grouped_s:.2} s with it");
    let out = std::env::temp_dir().join("names_groups_shelf.out");
    std::fs::write(&out, said).unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
}
