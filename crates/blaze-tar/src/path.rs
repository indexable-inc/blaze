//! An archive's paths in normal form: relative, `/`-separated, no empty, `.` or `..`
//! component, no trailing `/`, UTF-8. A path that cannot be put so is refused, never mended:
//! `..` is refused even where it would stay inside (`a/../b`), as a reader that took it would
//! have to trust the order of the directories it makes.

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_own::{Vec, planted::Planted};

use crate::{
    error::{PathWhy, Why},
    plant::TarPlant,
};

/// `raw` in normal form, written to `out` (cleared first). An empty result is the top
/// directory itself, which only a directory entry may name. `plant` is the seam.
pub(crate) fn normalize<A: Allocator>(
    raw: &[u8],
    most: u32,
    out: &mut Vec<u8, A>,
    plant: impl Planted<TarPlant>,
) -> Result<(), Why> {
    out.clear();
    if raw.len() > most as usize {
        return Err(Why::Path(PathWhy::TooLong { len: raw.len().min(u32::MAX as usize) as u32, most }));
    }
    syntax(raw)?;
    out.try_reserve(raw.len()).map_err(Why::Alloc)?;
    for part in raw.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." if !plant.has(TarPlant::ParentUnchecked) => return Err(Why::Path(PathWhy::Parent)),
            _ => {
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.extend_from_slice(part);
            }
        }
    }
    Ok(())
}

/// What any path or target may not hold.
fn syntax(raw: &[u8]) -> Result<(), Why> {
    let refuse = |why| Err(Why::Path(why));
    if raw.first() == Some(&b'/') {
        return refuse(PathWhy::Absolute);
    }
    if raw.contains(&0) {
        return refuse(PathWhy::Nul);
    }
    if raw.contains(&b'\\') {
        return refuse(PathWhy::Backslash);
    }
    if core::str::from_utf8(raw).is_err() {
        return refuse(PathWhy::NotUtf8);
    }
    Ok(())
}

/// A symbolic link's target, read from the directory of the link at `link` (normal form),
/// resolved by its components to a path from the top, written to `out`; refused if it leaves
/// the top. `through` says whether a path (a proper prefix of the result, as resolved so far)
/// is an earlier link, which resolving would follow.
pub(crate) fn resolve<A: Allocator>(
    link: &[u8],
    target: &[u8],
    most: u32,
    through: impl Fn(&[u8]) -> bool,
    out: &mut Vec<u8, A>,
) -> Result<(), Why> {
    out.clear();
    let as_target = |why: Why| match why {
        Why::Path(why) => Why::Target(why),
        other => other,
    };
    if target.is_empty() {
        return Err(Why::Target(PathWhy::Empty));
    }
    if target.len() > most as usize {
        return Err(Why::Target(PathWhy::TooLong { len: target.len().min(u32::MAX as usize) as u32, most }));
    }
    syntax(target).map_err(as_target)?;
    out.try_reserve(link.len() + target.len()).map_err(Why::Alloc)?;
    // The link's directory: its path less its last component.
    if let Some(slash) = link.iter().rposition(|&b| b == b'/') {
        out.extend_from_slice(&link[..slash]);
    }
    let parts = target.split(|&b| b == b'/').filter(|p| !p.is_empty() && *p != b".");
    let count = parts.clone().count();
    for (i, part) in parts.enumerate() {
        if part == b".." {
            if out.is_empty() {
                return Err(Why::TargetEscapes);
            }
            let cut = out.iter().rposition(|&b| b == b'/').unwrap_or(0);
            out.truncate(cut);
            continue;
        }
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend_from_slice(part);
        if i + 1 < count && through(out) {
            return Err(Why::ThroughLink);
        }
    }
    Ok(())
}

/// Whether `path` is `under` or a path inside it (both in normal form).
pub(crate) fn within(path: &[u8], under: &[u8]) -> bool {
    path.starts_with(under) && (path.len() == under.len() || path[under.len()] == b'/')
}
