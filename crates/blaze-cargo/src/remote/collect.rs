//! Collecting a remote: `blaze-engine cache gc --remote REMOTE`.
//!
//! A line is taken out of a manifest when its pack was written longer ago than asked, or
//! when every key of the pack is in other packs of the same manifest, which is what a
//! store that merged its small packs and pushed the merged one leaves behind. An object is
//! removed when no list names it and it was written longer ago than the time an unlisted
//! object is given. Nothing is removed for any other reason, so a line taken out in one
//! run has its object removed by that run or a later one, under the one rule.
//!
//! A push that is under way is safe from this by the clock: it sends again what has been
//! there for more than [`REUSE_FOR`], and an unlisted object is given at least twice that.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use anyhow::{Context, bail};
use futures_util::{StreamExt, TryStreamExt, stream};

use super::{
    Digest, LIST_MAX, Listed, Manifest, NotRewritten, REUSE_FOR, Remote, keys_object,
    transfer::Object,
};

/// The directories of a remote whose objects a list names.
const NAMED: [&str; 4] = ["packs/", "keys/", "units/", "index/"];

/// How long an object is given.
pub struct Ages {
    /// How long after it was written an object no list names is removed.
    pub unlisted: Duration,
    /// How long after it was written a pack is taken off the manifests, if at all.
    pub listed: Option<Duration>,
    /// Whether to only say what would be done.
    pub dry: bool,
}

impl Ages {
    /// The least an unlisted object can be given: see the module.
    const LEAST: Duration = Duration::from_secs(2 * REUSE_FOR.as_secs());
    pub const UNLISTED: Duration = Duration::from_secs(24 * 60 * 60);

    fn check(&self) -> anyhow::Result<()> {
        if self.unlisted < Ages::LEAST {
            bail!(
                "an unlisted object is given at least {} seconds: a push takes one that is \
                 there as sent for {} seconds after it was written",
                Ages::LEAST.as_secs(),
                REUSE_FOR.as_secs()
            );
        }
        Ok(())
    }
}

/// A time span as `90s`, `30m`, `12h` or `7d`.
pub fn parse_age(text: &str) -> Option<Duration> {
    let (count, unit) = text.split_at(text.len().checked_sub(1)?);
    let unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return None,
    };
    Some(Duration::from_secs(count.parse::<u64>().ok()?.checked_mul(unit)?))
}

fn is_older(object: &Object, than: Duration) -> bool {
    object.written.elapsed().is_ok_and(|age| age > than)
}

/// What a run took out.
#[derive(Default)]
struct Collected {
    old: usize,
    redundant: usize,
    untrusted: usize,
    removed: usize,
    removed_bytes: u64,
    rewritten: usize,
}

/// Collects the remote: see the module.
pub async fn collect(remote: &Remote, ages: &Ages) -> anyhow::Result<String> {
    remote.may_write()?;
    ages.check()?;
    let mut collected = Collected::default();
    let packs = by_name(remote.list("packs/").await?);
    for manifest in remote.list("manifests/").await? {
        // An index names packs of its manifest, and a push takes out what that no longer
        // lists.
        if manifest.name.ends_with(super::index::SUFFIX) {
            continue;
        }
        delist(remote, &manifest.name, &packs, ages, &mut collected).await?;
    }
    sweep(remote, ages, &mut collected).await?;
    Ok(collected.said(ages.dry))
}

fn by_name(objects: Vec<Object>) -> BTreeMap<String, Object> {
    objects.into_iter().map(|it| (it.name.clone(), it)).collect()
}

/// Takes the lines of one manifest out that are old or redundant.
async fn delist(
    remote: &Remote,
    name: &str,
    packs: &BTreeMap<String, Object>,
    ages: &Ages,
    collected: &mut Collected,
) -> anyhow::Result<()> {
    let manifest = match remote.read_list(name).await {
        Ok(Some(lines)) => Manifest::parse(&lines),
        Ok(None) => return Ok(()),
        Err(e) => {
            eprintln!("remote: {e:#}: its lines are left as they are");
            collected.untrusted += 1;
            return Ok(());
        }
    };
    let is_old = |(pack, listed): &(&Digest, &Listed)| {
        let object = packs.get(&listed.object(pack).0);
        ages.listed.is_some_and(|age| object.is_some_and(|it| is_older(it, age)))
    };
    let old: BTreeSet<Digest> = manifest.0.iter().filter(is_old).map(|it| *it.0).collect();
    let young = manifest.0.iter().filter(|it| !old.contains(it.0));
    let redundant = redundant(remote, young.map(|(pack, listed)| (*pack, *listed)).collect()).await;
    collected.old += old.len();
    collected.redundant += redundant.len();
    if ages.dry || (old.is_empty() && redundant.is_empty()) {
        return Ok(());
    }
    let shrink = |lines: &str| {
        let mut manifest = Manifest::parse(lines);
        manifest.0.retain(|pack, _| !old.contains(pack) && !redundant.contains(pack));
        manifest.lines()
    };
    match remote.shrink_list(name, shrink).await {
        Ok(()) => collected.rewritten += 1,
        // A signed list is not one a machine without a key takes lines out of.
        Err(e) if e.is::<NotRewritten>() => {
            eprintln!("remote: {e:#}");
            collected.untrusted += 1;
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

/// The packs every key of which is in another of `listed` that stays. The ones with the
/// fewest keys go first, so of a merged pack and what it was merged from, the merged one
/// stays. A pack whose keys cannot be had stays, and stands in for nothing.
async fn redundant(remote: &Remote, listed: Vec<(Digest, Listed)>) -> BTreeSet<Digest> {
    let asked = stream::iter(listed.into_iter().map(|(pack, listed)| async move {
        let most = listed.keys_max(remote);
        let keys = remote.read_named_by_hash(&keys_object(&listed.keys), &listed.keys, most).await;
        Some((pack, listed.format, keys.ok().filter(|it| !it.is_empty())?))
    }));
    let mut known: Vec<_> =
        asked.buffered(remote.jobs()).filter_map(|it| async move { it }).collect().await;
    known.sort_by_key(|(pack, _, keys)| (keys.len(), *pack));
    let mut holders: BTreeMap<(u32, &[u8]), usize> = BTreeMap::new();
    for (_, format, keys) in &known {
        for key in keys.chunks_exact(32) {
            *holders.entry((*format, key)).or_default() += 1;
        }
    }
    let mut redundant = BTreeSet::new();
    for (pack, format, keys) in &known {
        let held = |key: &[u8]| holders.get(&(*format, key)).copied().unwrap_or(0);
        if keys.chunks_exact(32).all(|key| held(key) > 1) {
            for key in keys.chunks_exact(32) {
                holders.entry((*format, key)).and_modify(|it| *it -= 1);
            }
            redundant.insert(*pack);
        }
    }
    redundant
}

/// Removes the objects no list names that have been there for longer than they are given.
/// The lists are read after the objects are listed and each object is looked at once more
/// before it goes, so one that a push wrote again in between stays.
async fn sweep(remote: &Remote, ages: &Ages, collected: &mut Collected) -> anyhow::Result<()> {
    let mut objects = Vec::new();
    for dir in NAMED {
        objects.extend(remote.list(dir).await?);
    }
    let named = named(remote).await?;
    let unlisted =
        objects.into_iter().filter(|it| !named.contains(&it.name) && is_older(it, ages.unlisted));
    let remove = |object: Object| async move {
        if !ages.dry {
            let still = remote.found(&object.name).await?;
            if !still.is_some_and(|it| is_older(&it, ages.unlisted)) {
                return Ok(None);
            }
            remote.delete(&object.name).await?;
        }
        anyhow::Ok(Some(object.len))
    };
    let removed: Vec<Option<u64>> =
        stream::iter(unlisted.map(remove)).buffer_unordered(remote.jobs()).try_collect().await?;
    collected.removed = removed.iter().flatten().count();
    collected.removed_bytes = removed.iter().flatten().sum();
    Ok(())
}

/// The names of the objects that the lists of the remote name, whoever signed them: a
/// list this machine does not trust still keeps what it names. A list that cannot be read
/// is an error, since what it names is then not known.
async fn named(remote: &Remote) -> anyhow::Result<BTreeSet<String>> {
    let mut named = BTreeSet::new();
    for list in remote.list("manifests/").await? {
        let lines = lines_of(remote, &list.name).await?;
        if list.name.ends_with(super::index::SUFFIX) {
            named.extend(super::index::named(remote, &lines).await?);
            continue;
        }
        for (pack, listed) in Manifest::parse(&lines).0 {
            named.insert(listed.object(&pack).0);
            named.insert(keys_object(&listed.keys));
        }
    }
    for list in remote.list("deps/").await? {
        let lines = lines_of(remote, &list.name).await?;
        named.extend(crate::deps::archives(&lines).map(|it| format!("units/{it}")));
    }
    Ok(named)
}

async fn lines_of(remote: &Remote, name: &str) -> anyhow::Result<String> {
    let read = remote.read_small(name, LIST_MAX).await?;
    let (bytes, _) = read.with_context(|| format!("`{name}` went away while it was read"))?;
    String::from_utf8(bytes).with_context(|| format!("`{name}` is not text"))
}

impl Collected {
    fn said(&self, dry: bool) -> String {
        let (lines, objects) = if dry {
            ("lines that would be taken out", "objects that would be removed")
        } else {
            ("lines taken out", "objects removed")
        };
        format!(
            "{lines}: {} old, {} of packs whose keys are all in other packs\n\
             manifests written: {}\n\
             manifests left that this machine does not trust: {}\n\
             {objects}: {}\n\
             bytes: {}",
            self.old,
            self.redundant,
            self.rewritten,
            self.untrusted,
            self.removed,
            self.removed_bytes
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_are_a_count_and_a_unit() {
        assert_eq!(parse_age("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_age("7d"), Some(Duration::from_secs(7 * 24 * 60 * 60)));
        assert_eq!(parse_age("7"), None);
        assert_eq!(parse_age("d"), None);
        assert_eq!(parse_age(""), None);
    }

    #[test]
    fn an_unlisted_object_is_given_longer_than_a_push_reuses_it() {
        let ages = |unlisted| Ages { unlisted, listed: None, dry: false };
        assert!(ages(REUSE_FOR).check().is_err());
        assert!(ages(Ages::UNLISTED).check().is_ok());
    }
}
