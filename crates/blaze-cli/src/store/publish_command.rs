//! `blaze store publish <folder> --signer <key file>` (D136): the toolchain's records this home
//! holds, laid out as the published set of this blaze's toolchain and target and written into a
//! folder origin, signed by the seed the key file holds.
//!
//! WHO. Only a trusted builder's blaze publishes: one built with blaze-cli's `publish` feature
//! (CI's), whose store also puts the toolchain's records `PUBLIC` ([`super::TRUSTED_BUILDER`],
//! D139). A developer's blaze refuses the command by name ([`PublishedError::NotPublisher`])
//! before it reads anything. An `http://` or `https://` origin is refused by name for everyone
//! ([`PublishedError::HttpOrigin`]): a shared origin is written by CI and the remote workers
//! alone, by their own upload.
//!
//! THE SEED. Read through the command's `Io` (the filesystem it grants), at most 66 bytes, into
//! a `Signer` that lives on this stack frame only. It is never printed, never in an error (a
//! bad key file is `SignerKey`, which shows no byte of it), and never in a record, the profile
//! or the origin: only the public key, inside the signed head and on the closing line, leaves.
//!
//! ORDER. Every object named by its digest first, then the `prefix`, then the `head` (store/
//! published.rs `write`, `Order::HeadLast`): a reader never sees a head whose objects are not
//! all there, whatever the moment a crash stops the write.

use bstd::cx::Cx;
use bstd::entry::Status;
use bstd::out::Out;

use super::published::PublishedError;
use crate::CliError;

/// Whether this blaze is a trusted builder's: what its build says, the one place it is chosen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Builder {
    /// A developer's blaze: it publishes nothing.
    Developer,
    /// CI's blaze, built with the `publish` feature.
    #[cfg(any(test, feature = "publish"))]
    Trusted,
}

/// This blaze's builder.
#[cfg(feature = "publish")]
pub(crate) const THIS_BUILDER: Builder = Builder::Trusted;
#[cfg(not(feature = "publish"))]
pub(crate) const THIS_BUILDER: Builder = Builder::Developer;

/// What `blaze store publish` takes: the folder and the key file.
const USAGE: &str = "blaze store publish <folder> --signer <key file>: the folder origin to write, and a file holding the signer's Ed25519 seed as 64 hex digits";

/// `blaze store …`: its one subcommand, `publish`, as this blaze's builder may run it.
#[expect(clippy::too_many_arguments, reason = "the publish command's edge: each argument one specific capability")]
pub(crate) fn run(
    cx: &Cx,
    args: &bstd::args::Args,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    clock: &bstd::clock::Clock,
    host: &bstd::host::Host,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    heap: &'static blaze_own::Heap,
) -> Result<Status, CliError> {
    let words = words(args.each().skip(2))?;
    admit(THIS_BUILDER, &words)?;
    match THIS_BUILDER {
        Builder::Developer => {
            let _ = (cx, fs, env, clock, host, rng, out, err, heap);
            Err(PublishedError::NotPublisher.into())
        }
        #[cfg(any(test, feature = "publish"))]
        Builder::Trusted => {
            // The home and store budget, read here at the edge (rule 75).
            let home = env.and_then(|env| env.get(&bstd::env::VarName::new("HOME").ok()?).ok().flatten());
            let max_store_bytes = crate::gc::budget_from_env(env)?;
            let opened = super::open_for_command(cx, fs, host, rng, home, max_store_bytes, &fs.root(), "", clock, err, heap)?;
            trusted::publish(cx, fs, out, &words, &opened.lent, blaze_toolchain::PINNED)
        }
    }
}

/// The publish's two words, parsed from the words after `store`.
pub(crate) struct Words<'a> {
    pub folder: &'a str,
    #[cfg_attr(not(any(test, feature = "publish")), allow(dead_code, reason = "read by a trusted builder's publish alone (the `publish` feature)"))]
    pub signer: &'a str,
}

pub(crate) fn words<'a>(mut each: impl Iterator<Item = &'a str>) -> Result<Words<'a>, CliError> {
    match each.next() {
        Some("publish") => {}
        _ => return Err(CliError::Usage("`blaze store` has one subcommand: `blaze store publish <folder> --signer <key file>`")),
    }
    let (mut folder, mut signer) = (None, None);
    while let Some(word) = each.next() {
        match word {
            "--signer" => match each.next() {
                Some(path) if signer.is_none() => signer = Some(path),
                Some(_) => return Err(CliError::Usage("`blaze store publish` takes one --signer")),
                None => return Err(CliError::Usage(USAGE)),
            },
            flag if flag.starts_with('-') => return Err(CliError::Usage(USAGE)),
            _ if folder.is_some() => return Err(CliError::Usage("`blaze store publish` takes one folder")),
            named => folder = Some(named),
        }
    }
    match (folder, signer) {
        (Some(folder), Some(signer)) => Ok(Words { folder, signer }),
        _ => Err(CliError::Usage(USAGE)),
    }
}

/// The refusals before anything is read: a developer's blaze publishes nothing; an HTTP origin
/// is written by no blaze.
pub(crate) fn admit(builder: Builder, words: &Words<'_>) -> Result<(), PublishedError> {
    if builder == Builder::Developer {
        return Err(PublishedError::NotPublisher);
    }
    super::published::folder_only(words.folder)?;
    Ok(())
}

#[cfg(any(test, feature = "publish"))]
pub(crate) mod trusted {
    use bstd::cx::Cx;
    use bstd::entry::Status;
    use bstd::fs::Fs;
    use bstd::out::Out;
    use bstd::path::Path;

    use super::super::CommandLent;
    use super::super::published::{self, folder};
    use super::Words;
    use crate::CliError;

    /// The publish of `toolchain`'s set from the home's store `lent`: the seed read, the
    /// toolchain's root walked, the set laid out and written head last; one line on `out`.
    pub(crate) fn publish(cx: &Cx, fs: &Fs, out: &Out, words: &Words<'_>, lent: &CommandLent, toolchain: blaze_toolchain::ToolchainId) -> Result<Status, CliError> {
        let origin = published::folder_only(words.folder)?;
        let signer = folder::signer_from_file(cx, &fs.root(), &Path::new(words.signer)?)?;
        let hex = toolchain.hex();
        let set = blaze_remote::published::set::SetName::new(hex.as_str(), crate::graph::TRIPLE).map_err(published::PublishedError::Set)?;
        let alloc = *lent.alloc();
        let mut walked = std::vec::Vec::new_in(alloc);
        published::walk_toolchain(lent, toolchain, &mut walked)?;
        let dir = fs.open_dir(cx, &Path::new(origin)?)?;
        let set_dir = folder::set_dir(cx, &dir, &set)?;
        let plan = folder::plan(cx, &set_dir, &set, &walked, &signer, alloc)?;
        let objects = if plan.unchanged { 0 } else { folder::write(cx, &set_dir, &plan, folder::Order::HeadLast, usize::MAX)? };
        let id = set.id();
        let [public, tool, target] = set.dir();
        out.line(
            cx,
            format_args!(
                "published: set {id} generation {}: {} records, {objects} objects written to {origin}/{public}/{tool}/{target}{}; {} left out (local); signed by {}",
                plan.generation,
                plan.published,
                if plan.unchanged { " (unchanged: its head is already these bytes)" } else { "" },
                plan.left_out,
                signer.public(),
            ),
        )?;
        Ok(Status::OK)
    }
}

/// The command's refusals (a developer's blaze, an http origin, a missing or bad key file), and
/// that no output, origin file or store file holds the seed.
#[cfg(test)]
#[path = "publish_command_tests.rs"]
mod tests;
