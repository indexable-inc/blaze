//! `ar` archives of ELF objects: GNU, GNU 64-bit and BSD.

use blaze_codegen::Arch;
use blaze_graph::par;
use object::read::archive::{ArchiveFile, ArchiveKind};

use super::strings::Table;
use crate::model::{Archive, Member, Name, Names};

pub(super) fn read(arch: Arch, name: &str, bytes: &[u8]) -> Result<Archive, String> {
    let error = |error: object::read::Error| format!("{name}: {error}"); // alloc: error path
    let limit = bytes.len();
    let file = ArchiveFile::parse(bytes).map_err(error)?;
    if file.is_thin() {
        // alloc: error path
        return Err(format!(
            "{name}: a thin archive, whose members are files outside it: blaze links only \
             archives that hold their members"
        ));
    }
    match file.kind() {
        ArchiveKind::Unknown
        | ArchiveKind::Gnu
        | ArchiveKind::Gnu64
        | ArchiveKind::Bsd
        | ArchiveKind::Bsd64 => {}
        kind => return Err(format!("{name}: a {kind:?} archive, not an archive of ELF objects")), // alloc: error path
    }

    let special = |name: &[u8]| matches!(name, b"/" | b"//" | b"/SYM64/") || name.starts_with(b"__.SYMDEF");
    // Every name of the index lies in its strings, each before a NUL: they are read as one
    // string table. Measured first, with the members, so each column is sized once.
    let symbols = file.symbols().map_err(error)?;
    let base = bytes.as_ptr().addr();
    let place = |name_bytes: &[u8]| name_bytes.as_ptr().addr().wrapping_sub(base);
    let (mut low, mut high, mut entries) = (bytes.len(), 0, 0);
    // clone: two slice iterators over the archive's bytes, nothing allocated
    for symbol in symbols.clone().into_iter().flatten() {
        let symbol = symbol.map_err(error)?;
        low = low.min(place(symbol.name()));
        high = high.max(place(symbol.name()).saturating_add(symbol.name().len() + 1));
        entries += 1;
    }
    let strings = bytes.get(low..high).unwrap_or_default();
    let (mut count, mut member_names) = (0, 0);
    for member in file.members() {
        let member = member.map_err(error)?;
        if !special(member.name()) {
            count += 1;
            member_names += member.name().len();
        }
    }

    // In archive order, so by the start of their bytes: a symbol finds its member by binary
    // search on that start, which is what its header's offset in the symbol table comes to.
    let mut names = Names::with_capacity(member_names.saturating_add(strings.len()).min(limit));
    let mut members: Vec<Member> = Vec::with_capacity(count);
    for member in file.members() {
        let member = member.map_err(error)?;
        if special(member.name()) {
            continue;
        }
        // alloc: borrows the name; a copy only for one that isn't UTF-8
        let member_name = String::from_utf8_lossy(member.name());
        let (offset, size) = member.file_range();
        let range = usize::try_from(offset)
            .ok()
            .zip(usize::try_from(size).ok())
            .and_then(|(start, size)| Some(start..start.checked_add(size)?))
            .filter(|range| range.end <= bytes.len())
            .ok_or_else(|| format!("{name}: member {member_name} runs past the archive's end"))?; // alloc: error path
        if members.last().is_some_and(|last| last.bytes.start >= range.start) {
            // alloc: error path
            return Err(format!("{name}: member {member_name} does not follow the member before it"));
        }
        members.push(Member { name: push(&mut names, &member_name, name, limit)?, bytes: range });
    }

    let mut index = Vec::with_capacity(entries);
    match symbols {
        Some(symbols) => {
            // A BSD index names its strings by offset, so many entries may name one string,
            // or offsets inside it; each is still pushed once.
            let mut table = Table::new(name, strings)?;
            // A member's symbols are listed together: its header is parsed once for the run.
            let mut last: Option<(u64, u32)> = None;
            for symbol in symbols {
                let symbol = symbol.map_err(error)?;
                let symbol_name = || String::from_utf8_lossy(symbol.name()); // alloc: error path
                let offset = symbol.offset();
                let member = match last {
                    Some((at, member)) if at == offset.0 => member,
                    _ => {
                        let header = file.member(offset).map_err(|error| {
                            // alloc: error path
                            format!("{name}: archive symbol {}: {error}", symbol_name())
                        })?;
                        let start = usize::try_from(header.file_range().0).ok();
                        let Some(member) = start
                            .and_then(|start| members.binary_search_by_key(&start, |it| it.bytes.start).ok())
                        else {
                            // alloc: error path
                            return Err(format!(
                                "{name}: archive symbol {} names a member at offset {}, which is \
                                 not a member file",
                                symbol_name(),
                                offset.0
                            ));
                        };
                        last = Some((offset.0, member as u32));
                        member as u32
                    }
                };
                // Inside `strings`, which were measured from these names: it fits a u32.
                let string = (place(symbol.name()) - low) as u32;
                let interned = table.name(&mut names, name, "the archive symbol", string)?;
                index.push((interned, member));
            }
        }
        None => {
            // What lld does without a symbol table: read every member, in parallel, and keep
            // member order. The member is named only in an error ([`in_member`]). One result
            // column for the archive, and one offsets column per member, each a file read once.
            let defined: Vec<Result<(&[u8], Vec<u32>), String>> = par::map(&members, |member| {
                let at = &bytes[member.bytes.start..member.bytes.end];
                super::defined(arch, "", at).map_err(in_member(name, &names, member))
            }); // alloc: once per archive without an index, in member order
            index.reserve(defined.iter().map(|it| it.as_ref().map_or(0, |(_, offsets)| offsets.len())).sum());
            // Then the names, in member order, each string of a member's `.strtab` pushed once:
            // the arena is at most the archive (`limit` holds without a check), and at most
            // the `.strtab`s, so it grows once, here, not by doubling.
            let strtabs: usize = defined.iter().map(|it| it.as_ref().map_or(0, |(strtab, _)| strtab.len())).sum();
            names.reserve(strtabs.min(limit.saturating_sub(names.bytes())));
            for (number, found) in defined.into_iter().enumerate() {
                let (strtab, offsets) = found?;
                let member = &members[number];
                let mut table = Table::new("", strtab).map_err(in_member(name, &names, member))?;
                for offset in offsets {
                    let symbol = table
                        .name(&mut names, "", "the symbol name", offset)
                        .map_err(in_member(name, &names, member))?;
                    index.push((symbol, number as u32));
                }
            }
        }
    }
    Ok(Archive { members, index, names })
}

/// Names the member in an error the member's reader made with an empty file name (`": why"`):
/// `archive(member): why`, as when the member is read on its own.
fn in_member<'a>(name: &'a str, names: &'a Names, member: &'a Member) -> impl FnOnce(String) -> String + 'a {
    // alloc: an error's message.
    move |why| format!("{name}({}){why}", names.at(member.name))
}

/// Pushes the member name `text` into `names`, refusing an arena bigger than the archive
/// (`limit` bytes): a real archive holds each member's name, so only a GNU long name that many
/// members point at could make it so.
fn push(names: &mut Names, text: &str, name: &str, limit: usize) -> Result<Name, String> {
    let pushed = names.push(text).map_err(super::full(name))?;
    if names.bytes() > limit {
        return Err(format!("{name}: its names come to more bytes than the archive holds")); // alloc: error path
    }
    Ok(pushed)
}
