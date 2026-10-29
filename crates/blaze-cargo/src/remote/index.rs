//! A project's key index on the remote, written by every push: which pack holds which key,
//! so that a store lacking a key can fetch the one pack that has it without first reading
//! the key list of every pack. The format, and all that decides what goes in it, is
//! `blaze_store::remote_index`; this reads and sends its objects.
//!
//! A push indexes every pack the manifest lists, of the format this build reads, that no
//! run of the index covers, its own and those other machines listed without indexing them
//! (a blaze from before the index), in one new run. It reads a pack's keys from the store
//! when the pack is here and from the pack's key list on the remote otherwise, so the first
//! push of a project that has a manifest and no index indexes all of it. The shards go up
//! before their run and the run before the root names it, so a reader never finds a name
//! with nothing under it. Runs of about one size are merged into one, without the packs the
//! manifest no longer lists. Two machines that push at once each add their run: a pack both
//! index is in two runs, which is harmless and gone at the next merge.
//!
//! A run covers packs of any format: a push leaves in the runs other builds wrote what the
//! manifest lists, and indexes only packs of the format it reads. A listed pack whose keys
//! cannot be read is not indexed and the push fails naming it, as every later push does
//! until its key list is mended or the pack is taken out of the manifest: a pack the index
//! does not cover is one no miss finds. A run that cannot be used is taken out, and what it
//! covered of this format indexed again: packs of another format wait for a push of a blaze
//! that reads them.

use std::{collections::BTreeSet, path::Path};

use anyhow::{Context, bail};
use blaze_store::{
    FORMAT as STORE_FORMAT,
    remote_index::{self as format, Builder, Hash, Named, Root, Run, Shard},
};
use futures_util::{StreamExt, TryStreamExt, stream};
use sha2::{Digest as _, Sha256};

use super::{Digest, Listed, Manifest, Remote, keys_object, keys_of};

/// What a project's name ends in to name its index: `manifests/<project>.index`.
pub const SUFFIX: &str = ".index";

/// The root of the index of `project`, a list signed as its manifest is.
pub fn root_at(project: &str) -> String {
    format!("manifests/{project}{SUFFIX}")
}

/// Where a run or a shard is: under the hash of its bytes.
pub fn object(hash: &Hash) -> String {
    format!("index/{}", format::hex(hash))
}

/// The bytes of the run or shard `hash` names, or why there are none to use: not there,
/// not what the hash says, or more than the `most` it can be. A failure to reach the
/// remote is an error.
async fn read(remote: &Remote, hash: &Hash, most: u64) -> anyhow::Result<Result<Vec<u8>, String>> {
    let name = object(hash);
    let read = match remote.read_small(&name, most.min(remote.object_max())).await {
        Ok(read) => read,
        Err(e) if e.is::<super::TooLong>() => return Ok(Err(format!("{e:#}"))),
        Err(e) => return Err(e),
    };
    let Some((bytes, _)) = read else { return Ok(Err(format!("{name} is not there"))) };
    let got = Digest::of(&bytes);
    if got.bytes() != hash {
        remote.traffic.rejected.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Ok(Err(format!("{name}: its {} bytes hash to {got}", bytes.len())));
    }
    Ok(Ok(bytes))
}

/// The run a root's line names, or why it cannot be used.
pub async fn run(remote: &Remote, named: &Named) -> anyhow::Result<Result<Run, String>> {
    let hash = &named.run;
    let read = read(remote, hash, format::run_max(named.packs)).await?;
    Ok(read.and_then(|bytes| {
        let run = Run::parse(&bytes).map_err(|why| format!("{} {why}", object(hash)))?;
        match run.packs().len() == named.packs as usize {
            true => Ok(run),
            false => Err(format!("{} is not of the packs its root says", object(hash))),
        }
    }))
}

/// The shard in `slot` of `run`, or why it cannot be used.
pub async fn shard(
    remote: &Remote,
    run: &Run,
    slot: usize,
    hash: &Hash,
) -> anyhow::Result<Result<Shard, String>> {
    Ok(read(remote, hash, run.shard_max(slot)).await?.and_then(|bytes| {
        Shard::parse(&bytes, run, slot).map_err(|why| format!("{} {why}", object(hash)))
    }))
}

/// The keys of a listed pack: from the store when it is here, otherwise from its key list
/// on the remote, checked against the hash the manifest names it by.
async fn keys(
    store: &Path,
    remote: &Remote,
    here: bool,
    pack: Digest,
    listed: Listed,
) -> anyhow::Result<Vec<u8>> {
    let keys = if here {
        let from = store.to_owned();
        tokio::task::spawn_blocking(move || keys_of(&from, pack)).await??.1
    } else {
        let most = listed.keys_max(remote);
        remote.read_named_by_hash(&keys_object(&listed.keys), &listed.keys, most).await?
    };
    if keys.len() % 32 != 0 {
        bail!("the keys of {pack}.pack are {} bytes, not keys of 32", keys.len());
    }
    Ok(keys)
}

/// Adds the keys of `packs` to `builder`, and says how many it added: a pack whose keys
/// cannot be read is in `failed` instead.
async fn index_packs(
    store: &Path,
    remote: &Remote,
    here: &BTreeSet<Digest>,
    packs: Vec<(Digest, Listed)>,
    builder: &mut Builder,
    failed: &mut Vec<anyhow::Error>,
) -> usize {
    let mut keys = stream::iter(packs.into_iter().map(|(pack, listed)| async move {
        (pack, keys(store, remote, here.contains(&pack), pack, listed).await)
    }))
    .buffer_unordered(remote.jobs());
    let mut indexed = 0;
    while let Some((pack, keys)) = keys.next().await {
        match keys {
            Ok(keys) => {
                builder.add_pack(pack.bytes(), keys.as_chunks::<32>().0);
                indexed += 1;
            }
            Err(e) => failed.push(e.context(format!("{pack}.pack is not indexed"))),
        }
    }
    indexed
}

/// Indexes what the manifest of `project` lists and the index does not cover, and merges
/// runs that are due: see the module. Says what it did, with the packs it could not index,
/// which the next push tries again.
pub async fn update(
    store: &Path,
    remote: &Remote,
    project: &str,
    here: &BTreeSet<Digest>,
) -> anyhow::Result<(String, Vec<anyhow::Error>)> {
    // The root before the manifest: every pack a run of it covers was listed before the run
    // was written, so it is in the manifest read after, unless a collection took it out.
    let name = root_at(project);
    let root = match remote.read_small(&name, super::LIST_MAX).await? {
        None => Root::default(),
        // Written anew, as `extend_list` writes a list this machine would not sign under.
        Some((bytes, _)) => match String::from_utf8(bytes)
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(remote.trust.open_to_extend(&name, &text)?.lines.to_owned()))
        {
            Ok(lines) => Root::parse(&lines),
            Err(e) => {
                eprintln!("remote: {e:#}: every pack is indexed as if there were no index");
                Root::default()
            }
        },
    };
    let manifest = Manifest::fetch_to_extend(remote, project).await?.unwrap_or_default();
    let listed = manifest.0;
    let is_listed = |pack: &Hash| listed.contains_key(&Digest::from(*pack));
    let read = stream::iter(
        root.runs
            .iter()
            .map(|named| async move { anyhow::Ok((named.clone(), run(remote, named).await?)) }),
    );
    let read: Vec<_> = read.buffered(remote.jobs()).try_collect().await?;
    let (mut kept, mut replaced) = (Vec::new(), BTreeSet::new());
    for (named, run) in read {
        match run {
            Ok(run) if run.packs().iter().any(is_listed) => kept.push((named, run)),
            // Of packs the manifest has all taken out.
            Ok(_) => _ = replaced.insert(named.run),
            Err(why) => {
                eprintln!(
                    "remote: the index of `{project}` names a run that cannot be used ({why}): \
                     it is taken out, and what it covered is indexed again"
                );
                replaced.insert(named.run);
            }
        }
    }
    let covered = |kept: &[(Named, Run)], skip: &BTreeSet<Hash>| -> BTreeSet<Hash> {
        let runs = kept.iter().filter(|(named, _)| !skip.contains(&named.run));
        runs.flat_map(|(_, run)| run.packs().iter().copied()).collect()
    };
    let to_index = |covered: &BTreeSet<Hash>| -> Vec<(Digest, Listed)> {
        let ours = listed.iter().filter(|(_, it)| it.format == STORE_FORMAT);
        ours.filter(|(pack, _)| !covered.contains(pack.bytes()))
            .map(|(pack, it)| (*pack, *it))
            .collect()
    };
    let mut builder = Builder::default();
    let mut failed = Vec::new();
    let new = to_index(&covered(&kept, &BTreeSet::new()));
    let mut indexed = index_packs(store, remote, here, new, &mut builder, &mut failed).await;
    let mut sizes: Vec<u64> = kept.iter().map(|(named, _)| named.entries).collect();
    if indexed != 0 {
        sizes.push(builder.len() as u64);
    }
    let mut merged: Vec<usize> =
        format::plan_merge(&sizes).into_iter().filter(|it| *it < kept.len()).collect();
    // A merge of one run into nothing new would only write it again.
    if indexed == 0 && merged.len() < 2 {
        merged.clear();
    }
    let mut broken = false;
    for &at in &merged {
        let (named, run) = &kept[at];
        let shards =
            stream::iter(run.shards().map(|(slot, hash)| async move {
                anyhow::Ok(shard(remote, run, slot, &hash).await?)
            }));
        let shards: Vec<_> = shards.buffered(remote.jobs()).try_collect().await?;
        match shards.into_iter().collect::<Result<Vec<_>, _>>() {
            Ok(shards) => builder.add_run(run, &shards, is_listed),
            Err(why) => {
                eprintln!(
                    "remote: the index of `{project}` names a shard that cannot be used ({why}): \
                     its run is taken out, and what it covered is indexed again"
                );
                broken = true;
            }
        }
        replaced.insert(named.run);
    }
    // What a run taken out covered, and no run left covers, from the key lists.
    if broken {
        let merging: BTreeSet<Hash> = merged.iter().map(|at| kept[*at].0.run).collect();
        let again = to_index(&covered(&kept, &merging));
        let again = again.into_iter().filter(|(pack, _)| !builder.covers(pack.bytes())).collect();
        indexed += index_packs(store, remote, here, again, &mut builder, &mut failed).await;
    }
    let made = match (indexed, merged.len()) {
        (0, 0) => None,
        _ => Some(builder.finish()).filter(|it| it.packs != 0),
    };
    if made.is_none() && replaced.is_empty() {
        let said = format!("index of `{project}`: {} runs, nothing to add", kept.len());
        return Ok((said, failed));
    }
    let added = match &made {
        Some(made) => {
            let send = |bytes: &Vec<u8>| {
                let hash: Hash = Sha256::digest(bytes).into();
                let bytes = bytes.clone();
                async move { remote.send_named_by_hash(&object(&hash), bytes).await }
            };
            stream::iter(made.shards.iter().map(send))
                .buffer_unordered(remote.jobs())
                .try_collect::<Vec<()>>()
                .await
                .context("sending the shards of the index")?;
            send(&made.run).await.context("sending a run of the index")?;
            let run = Sha256::digest(&made.run).into();
            Some(Named { run, entries: made.entries, packs: made.packs })
        }
        None => None,
    };
    let rewrite = |lines: &str| {
        let mut root = Root::parse(lines);
        root.runs.retain(|it| !replaced.contains(&it.run));
        if let Some(added) = &added
            && !root.runs.iter().any(|it| it.run == added.run)
        {
            root.runs.push(added.clone());
        }
        root.lines()
    };
    remote.extend_list(&name, rewrite).await?;
    let mut said = format!("index of `{project}`: {indexed} packs indexed");
    if !merged.is_empty() {
        said += &format!(", {} runs merged", merged.len());
    }
    if let Some(added) = &added {
        said += &format!(", a run of {} keys in {} packs added", added.entries, added.packs);
    }
    let runs = kept.len() - merged.len() + usize::from(added.is_some());
    said += &format!(", {runs} runs");
    Ok((said, failed))
}

/// The objects the root `lines` names, run and shards: what a collection of the remote
/// keeps. A run that cannot be used names no shard.
pub async fn named(remote: &Remote, lines: &str) -> anyhow::Result<Vec<String>> {
    let mut named = Vec::new();
    for listed in Root::parse(lines).runs {
        named.push(object(&listed.run));
        if let Ok(run) = run(remote, &listed).await? {
            named.extend(run.shards().map(|(_, hash)| object(&hash)));
        }
    }
    Ok(named)
}

/// What a store that lacks `key` would fetch: the packs of the index of `project` whose
/// entries match it, and that the manifest lists. For the tests, until the store reads
/// through the index.
#[cfg(test)]
pub async fn holders(
    remote: &Remote,
    project: &str,
    key: &[u8; 32],
) -> anyhow::Result<Vec<Digest>> {
    let manifest = Manifest::fetch(remote, project).await?.unwrap_or_default();
    let lines = remote.read_list(&root_at(project)).await?.unwrap_or_default();
    let mut holders = Vec::new();
    for listed in Root::parse(&lines).runs {
        let run = run(remote, &listed).await?.map_err(anyhow::Error::msg)?;
        let slot = run.slot(key);
        let Some(hash) = run.shard(slot) else { continue };
        let shard = shard(remote, &run, slot, &hash).await?.map_err(anyhow::Error::msg)?;
        let found = run.holders(&shard, key).map(|it| Digest::from(*it));
        holders.extend(found.filter(|it| manifest.0.contains_key(it)));
    }
    holders.sort();
    holders.dedup();
    Ok(holders)
}
