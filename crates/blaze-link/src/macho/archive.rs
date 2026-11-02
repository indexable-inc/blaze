//! Static archives as ld64 takes them: members found through the archive's symbol index.

#![forbid(unsafe_code)]

use blaze_ids::radix;
use object::read::archive::{ArchiveFile, ArchiveKind};

use crate::model::{Archive, Member, Name, Names};

/// A BSD `ar` archive (as Apple's `libtool`/`ar` write it, `#1/<len>` long names) or a GNU one,
/// with its `__.SYMDEF`/`__.SYMDEF SORTED`/`__.SYMDEF_64` index (a GNU `/` index too).
/// `Member::bytes` is the member's data, as a range of `bytes`.
///
/// Allocates six buffers, each sized exactly before it is filled (the members, their header
/// offsets and their ranges, the index, its entries by string offset, and the names), and the
/// radix sort's scratch when more than 64 entries' strings are out of offset order: none per
/// member, entry or name.
pub(super) fn read(name: &str, bytes: &[u8]) -> Result<Archive, String> {
    // alloc: error path
    let file = ArchiveFile::parse(bytes).map_err(|e| format!("{name}: {e}"))?;
    if file.is_thin() {
        // alloc: error path
        return Err(format!("{name}: a thin archive, whose members are other files"));
    }
    match file.kind() {
        ArchiveKind::Bsd | ArchiveKind::Bsd64 | ArchiveKind::Gnu | ArchiveKind::Gnu64 => {}
        ArchiveKind::Unknown => {
            // alloc: error path
            return Err(format!("{name}: an archive without a symbol index (run `ranlib` on it)"));
        }
        // alloc: error path
        other => return Err(format!("{name}: a {other:?} archive, not a BSD or GNU one")),
    }

    let symbols = file
        .symbols()
        // alloc: error path
        .map_err(|e| format!("{name}: {e}"))?
        // alloc: error path
        .ok_or_else(|| format!("{name}: an archive without a symbol index (run `ranlib` on it)"))?;

    // The members' count and names' length first, a walk of their headers, so the columns and
    // the arena are allocated once at their size.
    let (mut count, mut member_bytes) = (0, 0);
    for member in file.members() {
        let Ok(member) = member else { break };
        count += 1;
        member_bytes += member.name().len();
    }
    let mut members = Vec::with_capacity(count);
    // Where each member's header is, in archive order (so sorted): what the index's offsets
    // name, found by binary search without parsing the member again. 8 bytes a member.
    let mut heads: Vec<u64> = Vec::with_capacity(count);
    let mut ranges = Vec::with_capacity(count);
    for member in file.members() {
        // alloc: error path
        let member = member.map_err(|e| format!("{name}: {e}"))?;
        let header = member
            .header()
            // alloc: error path
            .ok_or_else(|| format!("{name}: a member without an `ar` header"))?;
        heads.push((std::ptr::from_ref(header) as usize - bytes.as_ptr() as usize) as u64);
        let (offset, size) = member.file_range();
        let range = usize::try_from(offset)
            .ok()
            .and_then(|start| Some(start..start.checked_add(usize::try_from(size).ok()?)?))
            .filter(|it| it.end <= bytes.len())
            // alloc: error path
            .ok_or_else(|| format!("{name}: a member past the end of the archive"))?;
        let member_name = std::str::from_utf8(member.name())
            // alloc: error path
            .map_err(|_| format!("{name}: a member whose name isn't UTF-8: {:?}", member.name()))?;
        ranges.push((member_name, range));
    }

    // A BSD `__.SYMDEF` names each entry's string by offset, so many entries may name one
    // string: each distinct string is one `Name`, or the arena grows with the entries times
    // the string's length. The entries are sorted by where their string is in `bytes` (16
    // bytes an entry), and each run of one offset pushes its name once: no hash of the file's
    // offsets, and the arena sized from the distinct strings before anything is pushed.
    let entries = symbols.size_hint().0;
    let mut by_offset: Vec<Entry> = Vec::with_capacity(entries);
    // Each entry's name until its run names it: an empty name of an empty arena, which
    // allocates nothing.
    let unnamed = Names::default().push("").expect("an empty name fits");
    let mut index: Vec<(Name, u32)> = Vec::with_capacity(entries);
    for symbol in symbols {
        // alloc: error path
        let symbol = symbol.map_err(|e| format!("{name}: {e}"))?;
        let found = heads.binary_search(&symbol.offset().0).map_err(|_| {
            // alloc: error path
            let shown = String::from_utf8_lossy(symbol.name());
            // alloc: error path
            format!("{name}: the index puts {shown} at offset {}, which is no member", symbol.offset().0)
        })?;
        let raw = symbol.name();
        let (Ok(nth), Ok(len)) = (u32::try_from(index.len()), u32::try_from(raw.len())) else {
            // alloc: error path
            return Err(format!("{name}: an index of more than 2^32 entries or bytes"));
        };
        let at = (raw.as_ptr() as usize).wrapping_sub(bytes.as_ptr() as usize) as u64;
        by_offset.push(Entry { at, nth, len });
        index.push((unnamed, found as u32));
    }
    radix::sort_copy_by_key(&mut by_offset, |it| it.at);

    // An offset inside another entry's string names a tail of it, and every entry of one string
    // ends where it does, so sorted by offset they are one run of one end. Of a string, the
    // entries whose name starts with `_` lose it, so each is a suffix of the earliest one's: one
    // push. One without `_` gains a `\x01` before its own first byte, which no other entry's name
    // holds: a push each. The arena is summed exactly before anything is pushed.
    let mut arena = member_bytes;
    for string in by_offset.chunk_by(|a, b| a.at + u64::from(a.len) == b.at + u64::from(b.len)) {
        let mut shared = false;
        for run in string.chunk_by(|a, b| a.at == b.at) {
            let entry = run[0];
            if bytes.get(entry.at as usize) == Some(&b'_') {
                if !shared {
                    arena += entry.len as usize - 1;
                    shared = true;
                }
            } else {
                arena += entry.len as usize + 1;
            }
        }
    }
    // Without `_`, names inside one string grow with the entries times its length: an archive
    // no `ranlib` writes, refused before its arena is allocated (each name in the file once, and
    // a `\x01` per entry, bound what a real index needs).
    if arena > bytes.len() + by_offset.len() {
        // alloc: error path
        return Err(format!(
            "{name}: the symbol index names {} strings at offsets inside one another, {arena} \
             bytes of names from a {}-byte archive",
            by_offset.len(),
            bytes.len()
        ));
    }
    let mut names = Names::with_capacity(arena);
    for (member_name, range) in ranges {
        // alloc: error path
        let member_name = names.push(member_name).map_err(|e| format!("{name}: {e}"))?;
        members.push(Member { name: member_name, bytes: range });
    }
    for string in by_offset.chunk_by(|a, b| a.at + u64::from(a.len) == b.at + u64::from(b.len)) {
        // The earliest `_` entry of this string, and its core name: what later ones are suffixes of.
        let mut shared: Option<(u64, Name)> = None;
        for run in string.chunk_by(|a, b| a.at == b.at) {
            // The index's strings are slices of `bytes`, which `object` read the index from.
            let (at, len) = (run[0].at as usize, run[0].len as usize);
            let underscore = bytes.get(at) == Some(&b'_');
            let core = match shared {
                Some((first, core)) if underscore => names
                    .suffix(core, (run[0].at - first) as u32)
                    .expect("a `_` is a character boundary inside the earlier name"),
                _ => {
                    let core = super::push_core_name(&mut names, &bytes[at..at + len])
                        // alloc: error path
                        .map_err(|e| format!("{name}: {e}"))?;
                    if underscore {
                        shared = Some((run[0].at, core));
                    }
                    core
                }
            };
            for entry in run {
                index[entry.nth as usize].0 = core;
            }
        }
    }
    Ok(Archive { members, index, names })
}

/// An index entry: where its string is in the archive, its place in the index and its
/// string's length. 16 bytes.
#[derive(Clone, Copy)]
struct Entry {
    at: u64,
    nth: u32,
    len: u32,
}

const _: () = assert!(size_of::<Entry>() == 16);

#[cfg(test)]
pub(super) mod tests {
    use super::read;
    use crate::model::Archive;

    fn header(out: &mut Vec<u8>, name: &str, size: usize) {
        out.extend_from_slice(format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", 0, 0, 0, 644).as_bytes());
    }

    /// A BSD member: a `#1/<len>` name padded with NULs to 8 bytes, as Apple's `ar` writes.
    fn bsd_member(out: &mut Vec<u8>, name: &str, data: &[u8]) {
        let padded = (name.len() + 1).next_multiple_of(8);
        header(out, &format!("#1/{padded}"), padded + data.len());
        out.extend_from_slice(name.as_bytes());
        out.resize(out.len() + padded - name.len(), 0);
        out.extend_from_slice(data);
        if out.len() % 2 == 1 {
            out.push(b'\n');
        }
    }

    /// `members` with a `__.SYMDEF` naming `symbols` (name, member index).
    pub(in crate::macho) fn bsd(members: &[(&str, &[u8])], symbols: &[(&str, usize)]) -> Vec<u8> {
        let mut strings = Vec::new();
        let mut entries = Vec::new();
        for (symbol, member) in symbols {
            entries.push((strings.len() as u32, *member));
            strings.extend_from_slice(symbol.as_bytes());
            strings.push(0);
        }
        bsd_raw(members, strings, &entries)
    }

    /// `members` with a `__.SYMDEF` of `entries` (string offset into `strings`, member index).
    pub(in crate::macho) fn bsd_raw(members: &[(&str, &[u8])], mut strings: Vec<u8>, entries: &[(u32, usize)]) -> Vec<u8> {
        strings.resize(strings.len().next_multiple_of(4), 0);
        let symdef_name = "__.SYMDEF SORTED";
        let symdef_padded = (symdef_name.len() + 1).next_multiple_of(8);
        let symdef_len = 4 + 8 * entries.len() + 4 + strings.len();
        let mut at = 8 + 60 + symdef_padded + symdef_len;
        at += at % 2;
        let mut offsets = Vec::new();
        for (name, data) in members {
            offsets.push(at);
            at += 60 + (name.len() + 1).next_multiple_of(8) + data.len();
            at += at % 2;
        }
        let mut symdef = Vec::new();
        symdef.extend_from_slice(&(8 * entries.len() as u32).to_le_bytes());
        for (strx, member) in entries {
            symdef.extend_from_slice(&strx.to_le_bytes());
            symdef.extend_from_slice(&(offsets[*member] as u32).to_le_bytes());
        }
        symdef.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        symdef.extend_from_slice(&strings);

        let mut out = b"!<arch>\n".to_vec();
        bsd_member(&mut out, symdef_name, &symdef);
        for ((name, data), offset) in members.iter().zip(&offsets) {
            assert_eq!(out.len(), *offset);
            bsd_member(&mut out, name, data);
        }
        out
    }

    fn core(name: &str) -> String {
        crate::macho::core_name(name.as_bytes()).unwrap()
    }

    fn member_names(archive: &Archive) -> Vec<&str> {
        archive.members.iter().map(|it| archive.name(it.name)).collect()
    }

    fn index(archive: &Archive) -> Vec<(String, u32)> {
        archive.index.iter().map(|&(name, member)| (archive.name(name).to_owned(), member)).collect()
    }

    #[test]
    fn bsd_archive_with_long_names_and_its_index() {
        let bytes = bsd(
            &[("a_rather_long_member_name.o", &b"first"[..]), ("b.o", &b"second!"[..])],
            &[("_one", 0), ("_two", 1), ("_three", 1)],
        );
        let archive = read("libx.a", &bytes).unwrap();
        assert_eq!(member_names(&archive), ["a_rather_long_member_name.o", "b.o"]);
        assert_eq!(&bytes[archive.members[0].bytes.clone()], b"first");
        assert_eq!(&bytes[archive.members[1].bytes.clone()], b"second!");
        assert_eq!(index(&archive), [(core("_one"), 0), (core("_two"), 1), (core("_three"), 1)]);
    }

    #[test]
    fn gnu_archive_and_its_index() {
        let members: [(&str, &[u8]); 2] = [("a.o", &b"aaaa"[..]), ("b.o", &b"bbb"[..])];
        let names = b"_f\0_g\0";
        let index_len = 4 + 4 * 2 + names.len();
        let mut at = 8 + 60 + index_len;
        at += at % 2;
        let first = at;
        at += 60 + members[0].1.len();
        at += at % 2;
        let second = at;

        let mut out = b"!<arch>\n".to_vec();
        header(&mut out, "/", index_len);
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&(first as u32).to_be_bytes());
        out.extend_from_slice(&(second as u32).to_be_bytes());
        out.extend_from_slice(names);
        if out.len() % 2 == 1 {
            out.push(b'\n');
        }
        for (name, data) in members {
            header(&mut out, &format!("{name}/"), data.len());
            out.extend_from_slice(data);
            if out.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        let archive = read("libx.a", &out).unwrap();
        assert_eq!(member_names(&archive), ["a.o", "b.o"]);
        assert_eq!(&out[archive.members[1].bytes.clone()], b"bbb");
        assert_eq!(index(&archive), [(core("_f"), 0), (core("_g"), 1)]);
    }

    #[test]
    fn seeded_indexes_name_the_members_they_were_written_for() {
        let mut rng = blaze_ids::rng::SplitMix64::new(0x2545_f491_4f6c_dd1d);
        let mut below = |n: u64| rng.below(n);
        for round in 0..64 {
            let count = 1 + below(20) as usize;
            let names: Vec<String> = (0..count)
                .map(|i| if below(2) == 0 { format!("m{i}.o") } else { format!("member_with_a_long_name_{round}_{i}.o") })
                .collect();
            let datas: Vec<Vec<u8>> = (0..count).map(|i| vec![i as u8; 1 + below(40) as usize]).collect();
            let symbols: Vec<(String, usize)> =
                (0..below(50)).map(|i| (format!("_s{i}"), below(count as u64) as usize)).collect();
            let members: Vec<(&str, &[u8])> =
                names.iter().zip(&datas).map(|(name, data)| (name.as_str(), data.as_slice())).collect();
            let entries: Vec<(&str, usize)> = symbols.iter().map(|(name, at)| (name.as_str(), *at)).collect();
            let bytes = bsd(&members, &entries);
            let archive = read("libseeded.a", &bytes).unwrap_or_else(|e| panic!("round {round}: {e}"));
            assert_eq!(member_names(&archive), names, "round {round}");
            for (member, data) in archive.members.iter().zip(&datas) {
                assert_eq!(&bytes[member.bytes.clone()], data.as_slice(), "round {round}");
            }
            let want: Vec<(String, u32)> = symbols.iter().map(|(name, at)| (core(name), *at as u32)).collect();
            assert_eq!(index(&archive), want, "round {round}");
            // Every name in the one arena: the members' and the index's, nothing else.
            let bytes_named: usize = names.iter().map(String::len).sum::<usize>()
                + want.iter().map(|(name, _)| name.len()).sum::<usize>();
            assert_eq!(archive.names.bytes(), bytes_named, "round {round}");
        }
    }

    #[test]
    fn an_index_entry_that_names_no_member_is_refused() {
        let mut bytes = bsd(&[("a.o", &b"aaaa"[..])], &[("_a", 0)]);
        // The `__.SYMDEF`'s one entry: its member offset, 8 bytes past the ranlib array's size.
        let at = 8 + 60 + ("__.SYMDEF SORTED".len() + 1).next_multiple_of(8) + 8;
        let offset = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        bytes[at..at + 4].copy_from_slice(&(offset + 2).to_le_bytes());
        let error = read("libx.a", &bytes).unwrap_err();
        assert!(error.contains("_a") && error.contains("no member"), "{error}");
    }

    #[test]
    fn entries_naming_one_string_share_its_name() {
        let long = format!("_{}", "x".repeat(4095));
        let symbols: Vec<(&str, usize)> = vec![(long.as_str(), 0); 640];
        let mut bytes = bsd(&[("a.o", &b"aaaa"[..])], &symbols);
        // Every entry's string offset (the first word of each) to the first string's.
        let ranlib = 8 + 60 + ("__.SYMDEF SORTED".len() + 1).next_multiple_of(8) + 4;
        for i in 0..symbols.len() {
            bytes[ranlib + 8 * i..ranlib + 8 * i + 4].copy_from_slice(&0u32.to_le_bytes());
        }
        let archive = read("libsame.a", &bytes).unwrap();
        assert_eq!(archive.index.len(), 640);
        assert!(archive.index.iter().all(|it| *it == archive.index[0]), "one `Name` for one string");
        assert_eq!(archive.name(archive.index[0].0), core(&long));
        assert_eq!(archive.names.bytes(), "a.o".len() + core(&long).len());
    }

    /// One string, `text`, named from each of `offsets` by an entry for member 0.
    fn one_string(text: &str, offsets: &[u32]) -> Vec<u8> {
        let mut strings = text.as_bytes().to_vec();
        strings.push(0);
        let entries: Vec<(u32, usize)> = offsets.iter().map(|&at| (at, 0)).collect();
        bsd_raw(&[("a.o", &b"aaaa"[..])], strings, &entries)
    }

    #[test]
    fn offsets_at_underscores_inside_a_string_are_suffixes_of_its_name() {
        // `_` at 0, 2 and 5.
        let archive = read("libtail.a", &one_string("_a_bc_d", &[5, 0, 2, 5])).unwrap();
        let want = [core("_d"), core("_a_bc_d"), core("_bc_d"), core("_d")];
        let got: Vec<String> = index(&archive).into_iter().map(|(name, _)| name).collect();
        assert_eq!(got, want);
        // One push for all four: the arena holds `a.o` and `a_bc_d`.
        assert_eq!(archive.names.bytes(), "a.o".len() + "a_bc_d".len());
    }

    #[test]
    fn offsets_inside_a_string_name_what_each_names_alone() {
        // Starting with `_` or not, at any offset: the names are those of each entry's string
        // read alone, the old reader's answer.
        let text = "x_y__z";
        for first in 0..text.len() as u32 {
            let offsets: Vec<u32> = (first..text.len() as u32).rev().chain(first..=first).collect();
            let archive = read("libmixed.a", &one_string(text, &offsets)).unwrap();
            let got: Vec<String> = index(&archive).into_iter().map(|(name, _)| name).collect();
            let want: Vec<String> = offsets.iter().map(|&at| core(&text[at as usize..])).collect();
            assert_eq!(got, want, "from {first}");
            let pushed: usize = {
                let mut distinct: Vec<u32> = offsets.clone();
                distinct.sort_unstable();
                distinct.dedup();
                let shared = distinct.iter().find(|&&at| text.as_bytes()[at as usize] == b'_');
                let alone = distinct.iter().filter(|&&at| text.as_bytes()[at as usize] != b'_');
                shared.map_or(0, |&at| text.len() - at as usize - 1)
                    + alone.map(|&at| text.len() - at as usize + 1).sum::<usize>()
            };
            assert_eq!(archive.names.bytes(), "a.o".len() + pushed, "from {first}");
        }
    }

    #[test]
    fn many_offsets_without_underscores_inside_one_string_are_refused() {
        // The fuzz seed `suffixes_of_one_string`: 640 names of about 4 KB from a 10 KB archive.
        let text = format!("_{}", "a".repeat(4095));
        let offsets: Vec<u32> = (0..640).collect();
        let bytes = one_string(&text, &offsets);
        let error = read("libsuffix.a", &bytes).unwrap_err();
        assert!(error.contains("libsuffix.a") && error.contains("inside one another"), "{error}");
    }

    #[test]
    fn an_archive_without_an_index_is_refused() {
        let mut out = b"!<arch>\n".to_vec();
        bsd_member(&mut out, "a.o", b"data");
        let error = read("libnoindex.a", &out).unwrap_err();
        assert!(error.contains("libnoindex.a") && error.contains("symbol index"), "{error}");
    }
}
