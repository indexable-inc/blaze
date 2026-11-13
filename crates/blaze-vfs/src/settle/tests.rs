//! A settle against a twin that never settles, under the simulation (the journal's effects on
//! `blaze_core::sim::Sim`, as the image's tests run them): over random histories of commits,
//! collections, images made durable, settles and restores, the settled VFS answers as its
//! twin, a reader pinned before a settle reads on as it did, only what changed since the image
//! is the tree's own, and a restart from either is the same VFS (rule 26).

use std::{
    collections::{BTreeMap, BTreeSet},
    panic::AssertUnwindSafe,
    sync::Arc,
};

use blaze_core::sim::Rng;
use blaze_ids::ContentId;

use super::{PLANT, Plant, Settled, Unsettled};
use crate::{
    ImageError, Pinned, Vfs,
    image::{
        self, Bytes,
        tests::{Act, act, apply, file, image_of, mapping, settle as drain, start, state, usable},
    },
    store::Source,
};

/// Sets the planted bug for as long as it lives; none again after, a panic's unwind too.
struct Planted;

impl Planted {
    fn new(plant: Plant) -> Planted {
        PLANT.with(|it| it.set(plant));
        Planted
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        PLANT.with(|it| it.set(Plant::None));
    }
}

/// Every file under `root` as `pin` reads it, by path.
fn pinned_files(pin: &Pinned<'_>, root: ContentId) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        let listed = pin.list(root, &dir).expect("a directory of a head");
        let names: Vec<(String, bool)> = listed.map(|(name, is)| (name.to_owned(), is)).collect();
        for (name, is_dir) in names {
            let path = if dir.is_empty() { name } else { format!("{dir}/{name}") };
            if is_dir {
                dirs.push(path);
                continue;
            }
            let read = pin.file(root, &path).expect("no corrupt group").expect("a file listed");
            out.insert(path, read.bytes.to_vec());
        }
    }
    out
}

/// The files of every head of `vfs`, as `pin` reads them.
fn heads_read(vfs: &Vfs, pin: &Pinned<'_>) -> Vec<(String, BTreeMap<String, Vec<u8>>)> {
    let heads = vfs.heads().into_iter();
    heads.map(|(name, root)| (name.to_owned(), pinned_files(pin, root))).collect()
}

/// A history of `seed` done to a VFS that writes images, settles on the last durable one and
/// restarts from it, and to a twin that never images: the first break of the invariant, else
/// how many settles it made.
fn settled_run(seed: u64) -> Result<usize, String> {
    let mut rng = Rng::new(seed ^ 0x5e77_1ed);
    let (mut vfs, mut sim) = start(seed);
    let (mut twin, mut twin_sim) = start(seed.wrapping_add(1));
    let mut image: Option<Vec<u8>> = None;
    let mut settles = 0;
    for step in 0..160 {
        let at = |what: &str| format!("seed {seed} step {step}: {what}");
        match rng.below(16) {
            // An image is written only of a quiet journal: what was committed is durable first.
            0 | 1 => {
                drain(&mut sim, &mut vfs);
                match image_of(&mut vfs) {
                    Ok((mark, bytes)) => {
                        if !vfs.image_durable(mark.name) {
                            return Err(at("the image just written is not the one staged"));
                        }
                        image = Some(bytes);
                    }
                    Err(ImageError::Busy) => {}
                    Err(error) => return Err(at(&error.to_string())),
                }
            }
            2 | 3 => {
                if let Some(bytes) = &image {
                    let reader = vfs.reader();
                    let pin = reader.pin();
                    let before = heads_read(&vfs, &pin);
                    vfs.settle(mapping(bytes)).map_err(|it| at(&it.to_string()))?;
                    settles += 1;
                    if heads_read(&vfs, &pin) != before {
                        return Err(at("a pin taken before the settle reads otherwise after it"));
                    }
                    drop(pin);
                    if heads_read(&vfs, &reader.pin()) != before {
                        return Err(at("a pin taken after the settle reads otherwise"));
                    }
                }
            }
            4 => {
                let image = image.as_deref().map(mapping);
                let (back, found) = Vfs::restore(file(), 0, &sim.contents(&file()), image);
                if !usable(&found.image) || found.skipped > 0 || found.heads_dropped > 0 {
                    return Err(at(&format!("a restore of the settled VFS: {found:?}")));
                }
                if state(&back) != state(&vfs) {
                    return Err(at("a restore of the settled VFS answers otherwise than it"));
                }
                vfs = back;
            }
            _ => {
                let act = act(&mut rng, step);
                if apply(&mut vfs, &act) != apply(&mut twin, &act) {
                    return Err(at("a step answered otherwise than in the twin"));
                }
            }
        }
        drain(&mut sim, &mut vfs);
        drain(&mut twin_sim, &mut twin);
        if state(&vfs) != state(&twin) {
            return Err(at("the settled VFS answers otherwise than its twin"));
        }
    }
    Ok(settles)
}

/// Whether some seed's run breaks its invariant (an `Err` or a panic) with `plant` set.
fn caught(plant: Plant, run: impl Fn(u64) -> Result<usize, String>) -> bool {
    let _planted = Planted::new(plant);
    (0..64).any(|seed| !matches!(std::panic::catch_unwind(AssertUnwindSafe(|| run(seed))), Ok(Ok(_))))
}

/// The invariant: after every step of a history of commits, collections, images made durable,
/// settles on the last durable image and restores, the settled VFS answers as a twin that
/// never imaged (every head, file, listing and diff), as does a restore of it; and a reader
/// pinned before a settle reads every file of every head after it byte for byte.
#[test]
fn a_settled_tree_reads_what_it_read() {
    let mut settles = 0;
    for seed in 0..64 {
        settles += settled_run(seed).unwrap_or_else(|it| panic!("{it}"));
    }
    assert!(settles >= 64, "the histories settled {settles} times");
}

/// The control: a settle that leaves the heads and views at their old numbers (a collection
/// between the image and the settle moved them) is caught.
#[test]
fn the_oracle_catches_a_settle_that_keeps_its_numbers() {
    assert!(caught(Plant::KeepsItsNumbers, settled_run), "no seed caught the old numbers");
}

/// A tree of `seed`'s history, an image of it made durable, then more commits (no
/// collection, which would drop what no head reaches); then the settle, with what it owes
/// checked: the first break, else what the settle said.
fn holds_only_run(seed: u64) -> Result<Settled, String> {
    let at = |what: &str| format!("seed {seed}: {what}");
    let mut rng = Rng::new(seed ^ 0x401d);
    let (mut vfs, mut sim) = start(seed);
    for step in 0..40 {
        apply(&mut vfs, &act(&mut rng, step));
        drain(&mut sim, &mut vfs);
    }
    // A file made before the image whose bytes only the tree holds.
    let early: Arc<[u8]> = Arc::from(&b"made before the image, held by the tree alone"[..]);
    let weak = Arc::downgrade(&early);
    vfs.tree.file(Bytes::from(early));
    drain(&mut sim, &mut vfs);
    let (mark, bytes) = image_of(&mut vfs).map_err(|it| at(&it.to_string()))?;
    if !vfs.image_durable(mark.name) {
        return Err(at("the image just written is not the one staged"));
    }
    drain(&mut sim, &mut vfs);
    for step in 40..80 {
        let act = act(&mut rng, step);
        if !matches!(act, Act::Collect) {
            apply(&mut vfs, &act);
        }
        drain(&mut sim, &mut vfs);
    }
    let opened = image::open(mapping(&bytes)).map_err(|it| at(&format!("{it:?}")))?;
    let base = opened.tree.table();
    let imaged: BTreeSet<ContentId> = (0..base.base_len()).map(|node| *base.id(node)).collect();
    let table = vfs.tree.table();
    let mut owed = 0;
    for node in 0..table.len() {
        let shape = table.shape(node);
        if !shape.is_dir()
            && let Source::Heap(bytes) = table.source(node)
            && imaged.contains(table.id(node))
        {
            owed += bytes.len() as u64;
        }
    }
    let reader = vfs.reader();
    let pin = reader.pin();
    let settled = vfs.settle(mapping(&bytes)).map_err(|it| at(&it.to_string()))?;
    if settled.freed != owed {
        return Err(at(&format!("freed {} bytes, owed {owed}", settled.freed)));
    }
    let table = vfs.tree.table();
    for node in 0..table.len() {
        let shape = table.shape(node);
        if shape.is_dir() {
            continue;
        }
        let heap = matches!(table.source(node), Source::Heap(_));
        if heap == imaged.contains(table.id(node)) {
            return Err(at(&format!("node {node}: its own copy is {heap}, the image's not")));
        }
    }
    // The old table, and the bytes only it holds, outlive a reader pinned before.
    for _ in 0..8 {
        vfs.tree.reclaim();
    }
    if weak.upgrade().is_none() {
        return Err(at("a file a reader pinned before the settle can read was freed"));
    }
    drop(pin);
    // Other tests' readers may be in tasks begun before the settle; they end.
    for _ in 0..100_000 {
        vfs.tree.reclaim();
        if weak.strong_count() == 0 {
            break;
        }
        std::thread::yield_now();
    }
    if weak.strong_count() > 0 {
        return Err(at("the tree still holds its own copy of a file the image has"));
    }
    Ok(settled)
}

/// The invariant: after a settle and a bounded reclaim with no earlier pin left, a file's
/// bytes are the tree's own (`Table::source` is `Heap`) exactly when the image lacks its ID,
/// `freed` is the bytes of the others' own copies, and a file made before the image whose
/// bytes only the tree held is freed; while a reader pinned before the settle is held, it is
/// not.
#[test]
fn a_settled_tree_holds_only_what_changed_since() {
    for seed in 0..16 {
        let settled = holds_only_run(seed).unwrap_or_else(|it| panic!("{it}"));
        assert!(settled.freed > 0, "seed {seed}: a settle with nothing to free");
    }
}

/// The control: a settle that answers `Ok` and frees nothing is caught.
#[test]
fn the_oracle_catches_a_settle_that_frees_nothing() {
    let _planted = Planted::new(Plant::FreesNothing);
    let caught = (0..16).filter(|&seed| holds_only_run(seed).is_err()).count();
    assert_eq!(caught, 16, "every seed catches a settle that frees nothing");
}

/// Twins of one history, both writing their images at the same steps, one settling on each
/// as soon as it is durable: the first break, else how many images were settled on.
fn twins_run(seed: u64) -> Result<usize, String> {
    let mut rng = Rng::new(seed ^ 0x7a1e);
    let (mut settled, mut sim) = start(seed);
    let (mut plain, mut plain_sim) = start(seed);
    let mut last: Option<Vec<u8>> = None;
    let mut images = 0;
    for step in 0..160 {
        let at = |what: &str| format!("seed {seed} step {step}: {what}");
        if rng.below(12) == 0 {
            drain(&mut sim, &mut settled);
            drain(&mut plain_sim, &mut plain);
            match (image_of(&mut settled), image_of(&mut plain)) {
                (Ok((mark, bytes)), Ok((twin, twin_bytes))) => {
                    if (mark, &bytes) != (twin, &twin_bytes) {
                        return Err(at("the image a settled tree writes is not its twin's"));
                    }
                    if !settled.image_durable(mark.name) || !plain.image_durable(twin.name) {
                        return Err(at("the image just written is not the one staged"));
                    }
                    drain(&mut sim, &mut settled);
                    drain(&mut plain_sim, &mut plain);
                    settled.settle(mapping(&bytes)).map_err(|it| at(&it.to_string()))?;
                    images += 1;
                    last = Some(bytes);
                }
                (Err(ImageError::Busy), Err(ImageError::Busy)) => {}
                (a, b) => return Err(at(&format!("the twins' images: {a:?} and {b:?}"))),
            }
        } else {
            let act = act(&mut rng, step);
            if apply(&mut settled, &act) != apply(&mut plain, &act) {
                return Err(at("a step answered otherwise than in the twin"));
            }
        }
        drain(&mut sim, &mut settled);
        drain(&mut plain_sim, &mut plain);
        if sim.contents(&file()) != plain_sim.contents(&file()) {
            return Err(at("the settled tree's journal is not its twin's, byte for byte"));
        }
    }
    let image = || last.as_deref().map(mapping);
    let (back, found) = Vfs::restore(file(), 0, &sim.contents(&file()), image());
    let (twin_back, twin_found) = Vfs::restore(file(), 0, &plain_sim.contents(&file()), image());
    if !usable(&found.image) || !usable(&twin_found.image) {
        return Err(format!("seed {seed}: the restores: {found:?} and {twin_found:?}"));
    }
    let want = state(&plain);
    if state(&back) != want || state(&twin_back) != want || state(&settled) != want {
        return Err(format!("seed {seed}: a restart from the settled tree's image is another VFS"));
    }
    Ok(images)
}

/// The invariant (rule 26): of twins with one history, one settling on each image as soon as
/// it is durable, the journals' bytes and every image each writes are equal byte for byte,
/// and `Vfs::restore` of either equals both.
#[test]
fn a_restart_from_the_image_a_tree_settled_on_is_one_that_never_settled() {
    let mut images = 0;
    for seed in 0..32 {
        images += twins_run(seed).unwrap_or_else(|it| panic!("{it}"));
    }
    assert!(images >= 32, "the histories settled on {images} images");
}

/// The invariant: a settle names the image it refuses and the image the journal starts from,
/// and changes nothing: an image only staged, and an image of another tree. Once told the
/// image is durable, the same bytes settle.
#[test]
fn an_image_the_journal_was_not_told_is_durable_is_refused_by_name() {
    let mut rng = Rng::new(7);
    let (mut vfs, mut sim) = start(7);
    for step in 0..20 {
        apply(&mut vfs, &act(&mut rng, step));
        drain(&mut sim, &mut vfs);
    }
    let before = state(&vfs);
    let (mark, bytes) = image_of(&mut vfs).expect("a quiet journal takes an image");
    let refused = vfs.settle(mapping(&bytes)).expect_err("an image only staged");
    assert!(
        matches!(refused, Unsettled::NotDurable { image, base: None } if image == mark.name),
        "{refused:?}"
    );
    assert!(refused.to_string().contains(&format!("{:x}", mark.name)), "{refused}");
    assert_eq!(state(&vfs), before);
    let (mut other, mut other_sim) = start(8);
    apply(&mut other, &act(&mut Rng::new(8), 0));
    drain(&mut other_sim, &mut other);
    let (theirs, their_bytes) = image_of(&mut other).expect("a quiet journal takes an image");
    assert!(other.image_durable(theirs.name));
    let refused = vfs.settle(mapping(&their_bytes)).expect_err("an image of another tree");
    assert!(refused.to_string().contains(&format!("{:x}", theirs.name)), "{refused}");
    assert!(vfs.image_durable(mark.name));
    drain(&mut sim, &mut vfs);
    let settled = vfs.settle(mapping(&bytes)).expect("a durable image");
    assert_eq!(settled.nodes, mark.nodes);
    assert_eq!(state(&vfs), before);
}
