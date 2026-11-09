//! The code map of each link while `<target-dir>/perf` exists (docs/v2/plan/design/perf-history.md,
//! "The code map"): what each address of the image is, by path and by code identity, and what
//! it refers to, so `blaze perf record` names a run's functions across builds and
//! `blaze perf diff` tells a function whose code changed from one whose callee did.
//!
//! A node is each function and static of the build's reach graph (its edges are its references:
//! the callees its code calls, inlines or builds in, and the statics it reads), then each
//! function of the image the graph has no node of (a folded group's body), by its key. With no
//! `perf` directory nothing is written and a link costs nothing more.

use std::path::Path;

use blaze_codegen::{InstanceKey, driver::Built};
use blaze_ids::{ContentId, Hasher};
use blaze_link::Image;
use blaze_profile::perf::{
    hex,
    map::{self, Map, Node},
};

use super::Job;

/// The directory in the target directory.
pub(crate) const DIR: &str = "perf";
const CODE_DOMAIN: &str = "blaze v2 perf code 1";

/// The code of a node of two marks: a function's code hash and arguments, a static's value and
/// instance.
fn code(marks: &[ContentId; 2]) -> [u8; 32] {
    let mut hasher = Hasher::new(CODE_DOMAIN);
    hasher.id(&marks[0]).id(&marks[1]);
    hasher.finish().0
}

/// Writes the code map of `image`, just linked at `path` from `built`, when the target
/// directory has a `perf` directory.
pub(crate) fn write_map(job: &Job, path: &str, image: &Image, built: &Built) -> Result<(), String> {
    let perf = Path::new(&job.units().target_dir).join(DIR);
    if !perf.try_exists().map_err(|error| format!("{}: {error}", perf.display()))? {
        return Ok(());
    }
    let span = tracing::info_span!(target: "blaze", "perf.map", image = path, nodes = tracing::field::Empty);
    let _entered = span.enter();
    let (keys, starts, edges, marks) = built.reach.graph();
    let reached = keys.len();
    // clone: once per link that keeps a code map, the build's identities to sort by key
    let mut identities = built.identities.clone();
    identities.sort_unstable();
    // alloc: once per link that keeps a code map, the build's names by key
    let mut names: Vec<(&InstanceKey, &str)> =
        built.debug_names.iter().map(|it| (&it.0, it.1.as_str())).collect();
    names.sort_unstable();
    // alloc: once per link that keeps a code map, the image's functions, by key
    let mut placed: Vec<(InstanceKey, u64, u64)> = image.functions();
    placed.sort_unstable();
    let identity = |key: &InstanceKey| {
        let at = identities.binary_search_by(|it| it.0.cmp(key)).ok()?;
        Some(identities[at].1.0)
    };
    let name = |key: &InstanceKey| match names.binary_search_by(|it| it.0.cmp(key)) {
        // alloc: once a node
        Ok(at) => names[at].1.to_owned(),
        // alloc: once a node
        Err(_) => format!("<instance {:x}>", key.0),
    };
    let place = |key: &InstanceKey| match placed.binary_search_by(|it| it.0.cmp(key)) {
        Ok(at) => (placed[at].1, placed[at].2),
        // Reached but given no code of its own: a member of a folded group, or code the
        // link left out.
        Err(_) => (0, 0),
    };
    let edges_of = |node: usize| {
        let (from, to) = (starts[node] as usize, starts[node + 1] as usize);
        // alloc: once a node, its edges
        let mut it = edges[from..to].to_vec();
        it.sort_unstable();
        it.dedup();
        it
    };
    // alloc: once per link that keeps a code map
    let mut nodes: Vec<Node> = Vec::with_capacity(marks.len() + placed.len());
    for (at, key) in keys.iter().enumerate() {
        let (address, size) = place(key);
        nodes.push(Node {
            address,
            size,
            identity: identity(key),
            key: key.0.0,
            code: code(&marks[at]),
            name: name(key),
            edges: edges_of(at),
        });
    }
    for at in reached..marks.len() {
        let code = code(&marks[at]);
        nodes.push(Node {
            address: 0,
            size: 0,
            identity: None,
            key: code,
            code,
            // alloc: once a static
            name: format!("<static {:x}>", marks[at][1]),
            edges: edges_of(at),
        });
    }
    // The image's code no node holds: a folded group's body, named by its key.
    for (key, address, size) in &placed {
        if *size == 0 || keys.binary_search(key).is_ok() {
            continue;
        }
        nodes.push(Node {
            address: *address,
            size: *size,
            identity: identity(key),
            key: key.0.0,
            code: key.0.0,
            name: match names.binary_search_by(|it| it.0.cmp(key)) {
                // alloc: once a node
                Ok(at) => names[at].1.to_owned(),
                // alloc: once a node
                Err(_) => format!("<folded group {:x}>", key.0),
            },
            // alloc: none (an empty `Vec`): a folded body's references are its members'
            edges: Vec::new(),
        });
    }
    span.record("nodes", nodes.len() as u64);
    // alloc: once per link that keeps a code map
    map::write(&perf, &hex(&image.build_id()), &Map { image: path.to_owned(), nodes })
}
