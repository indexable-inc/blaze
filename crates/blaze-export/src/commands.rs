//! Mach-O load commands as a typed list (`<mach-o/loader.h>`), the one home for both writers:
//! the executable (macho.rs `write_native`) and the native dylib (native_dylib.rs `write`)
//! build their [`Commands`] first, then [`Commands::write`] writes the header and every command.
//!
//! Nothing is counted or sized by hand. `ncmds` is the list's length; `sizeofcmds` is the sum of
//! each command's [`Command::size`] (a segment's [`segment_size`]), the same functions the
//! writer's bytes follow, so a command's bytes are its `cmdsize` by construction and every
//! `cmdsize` is a multiple of 8 (names NUL-padded to 8). The room the layout kept for the header
//! (where the first section starts) is checked against the list in every build: past it is
//! [`CommandsError::Room`], never a debug assertion. A value a field cannot hold (a 32-bit
//! offset, a 16-byte name) is [`CommandsError::Field`], naming the field.

use blaze_own::{AVec, Alloc, Vec};

use crate::bytes;
use crate::macho;

/// `mach_header_64`.
pub(crate) const HEADER: u64 = 32;
/// `segment_command_64`, without its sections.
pub(crate) const SEGMENT: u64 = 72;
/// `section_64`.
pub(crate) const SECTION: u64 = 80;
/// `dylib_command::name.offset`: the name follows the 24-byte command.
pub(crate) const DYLIB_NAME_AT: u64 = 24;
/// `dylinker_command::name.offset`: the name follows the 12-byte command.
const DYLINKER_NAME_AT: u64 = 12;
/// `dysymtab_command`'s fields after `cmd` and `cmdsize`.
pub(crate) const DYSYMTAB_FIELDS: usize = 18;
/// `uuid_command`'s bytes.
const UUID_LEN: usize = 16;

const fn up8(n: u64) -> u64 {
    (n + 7) & !7
}

/// A segment's `cmdsize` with `nsects` sections.
pub(crate) const fn segment_size(nsects: u64) -> u64 {
    SEGMENT + SECTION * nsects
}

/// `segment_command_64`'s fields but `nsects`, which [`Commands::segment`] counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Segment<'n> {
    pub name: &'n str,
    pub vmaddr: u64,
    pub vm_size: u64,
    pub file: u64,
    pub file_size: u64,
    /// `maxprot` and `initprot`.
    pub prot: u32,
    pub flags: u32,
}

/// `section_64`: `addr` its address, `offset` its file offset (0 for zero fill).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Section<'n> {
    pub name: &'n str,
    pub seg: &'n str,
    pub addr: u64,
    pub size: u64,
    pub offset: u64,
    pub align: u32,
    pub flags: u32,
    pub reserved1: u32,
}

/// Every load command but a segment: a segment joins the list with its sections
/// ([`Commands::segment`]), so its `nsects` is their count by construction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Command<'n> {
    /// `linkedit_data_command`; `cmd` says which (chained fixups, exports trie, function
    /// starts, code signature).
    LinkeditData { cmd: u32, off: u64, size: u64 },
    /// `symtab_command`.
    Symtab { symoff: u64, nsyms: u64, stroff: u64, strsize: u64 },
    /// `dysymtab_command`: ilocalsym, nlocalsym, iextdefsym, nextdefsym, iundefsym, nundefsym,
    /// tocoff, ntoc, modtaboff, nmodtab, extrefsymoff, nextrefsyms, indirectsymoff,
    /// nindirectsyms, extreloff, nextrel, locreloff, nlocrel.
    Dysymtab { fields: [u64; DYSYMTAB_FIELDS] },
    /// `dylinker_command` (`LC_LOAD_DYLINKER`).
    Dylinker { name: &'n str },
    /// `uuid_command`: zeros until the writer knows the content ([`Written::uuid_at`]).
    Uuid,
    /// `build_version_command` with no tools.
    BuildVersion { platform: u32, minos: u32, sdk: u32 },
    /// `entry_point_command` (`LC_MAIN`).
    Main { entry: u64, stack: u64 },
    /// `dylib_command`; `cmd` is `LC_ID_DYLIB` or `LC_LOAD_DYLIB`.
    Dylib { cmd: u32, name: &'n str, timestamp: u32, current: u32, compat: u32 },
}

impl Command<'_> {
    /// Its `cmdsize`, which is what [`Commands::write`] writes for it: a multiple of 8.
    pub(crate) fn size(&self) -> u64 {
        match *self {
            Command::LinkeditData { .. } => 16,
            Command::Symtab { .. } => 24,
            Command::Dysymtab { .. } => 8 + 4 * DYSYMTAB_FIELDS as u64,
            Command::Dylinker { name } => up8(DYLINKER_NAME_AT + name.len() as u64 + 1),
            Command::Uuid => 8 + UUID_LEN as u64,
            Command::BuildVersion { .. } => 24,
            Command::Main { .. } => 24,
            Command::Dylib { name, .. } => DYLIB_NAME_AT + up8(name.len() as u64 + 1),
        }
    }
}

#[derive(Clone, Copy)]
enum Item<'n> {
    Segment { segment: Segment<'n>, first: usize, count: usize },
    Other(Command<'n>),
}

/// The load commands in file order, and the sections of their segments.
pub(crate) struct Commands<'n, 'a> {
    items: AVec<'a, Item<'n>>,
    sections: AVec<'a, Section<'n>>,
}

/// `mach_header_64`'s fields a writer chooses.
#[derive(Clone, Copy)]
pub(crate) struct Header {
    pub filetype: u32,
    pub flags: u32,
}

/// What the writer fills once it knows the content.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Written {
    /// `LC_UUID`'s 16 bytes, at this offset of `out`.
    pub uuid_at: usize,
}

/// Why the load commands could not be written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CommandsError {
    /// The header and load commands need `need` bytes; the layout kept `room` (where the first
    /// section starts).
    Room { need: u64, room: u64 },
    /// `value` does not fit field `what`.
    Field { what: &'static str, value: u64 },
}

fn field(what: &'static str, value: u64) -> Result<u32, CommandsError> {
    u32::try_from(value).map_err(|_| CommandsError::Field { what, value })
}

fn name16(out: &mut AVec<'_, u8>, name: &str, what: &'static str) -> Result<(), CommandsError> {
    if name.len() > 16 {
        return Err(CommandsError::Field { what, value: name.len() as u64 });
    }
    bytes::name16(out, name);
    Ok(())
}

/// `name` then zeros to `len` bytes (a command's size makes `len` at least one past the name).
fn padded(out: &mut AVec<'_, u8>, name: &str, len: u64) {
    let at = out.len();
    out.extend_from_slice(name.as_bytes());
    out.resize(at + len as usize, 0);
}

impl<'n, 'a> Commands<'n, 'a> {
    pub(crate) fn new(alloc: Alloc<'a>) -> Self {
        Commands { items: Vec::new_in(alloc), sections: Vec::new_in(alloc) }
    }

    /// `segment` with `sections`, its `nsects` their count.
    pub(crate) fn segment(&mut self, segment: Segment<'n>, sections: impl IntoIterator<Item = Section<'n>>) {
        let first = self.sections.len();
        self.sections.extend(sections);
        self.items.push(Item::Segment { segment, first, count: self.sections.len() - first });
    }

    pub(crate) fn push(&mut self, command: Command<'n>) {
        self.items.push(Item::Other(command));
    }

    /// `ncmds`.
    pub(crate) fn count(&self) -> usize {
        self.items.len()
    }

    /// `sizeofcmds`: every command's `cmdsize`, summed.
    pub(crate) fn size(&self) -> u64 {
        self.items
            .iter()
            .map(|item| match *item {
                Item::Segment { count, .. } => segment_size(count as u64),
                Item::Other(command) => command.size(),
            })
            .sum()
    }

    /// Appends the header and every command to `out`, which is empty (the header starts the
    /// file). Refuses first, in every build, a list whose header and commands pass `room`; and a
    /// list with no `LC_UUID`, whose bytes the writer fills.
    pub(crate) fn write(&self, out: &mut AVec<'_, u8>, header: Header, room: u64) -> Result<Written, CommandsError> {
        let size = self.size();
        let need = HEADER + size;
        if need > room {
            return Err(CommandsError::Room { need, room });
        }
        bytes::u32(out, macho::MH_MAGIC_64);
        bytes::u32(out, macho::CPU_TYPE_ARM64);
        bytes::u32(out, 0); // CPU_SUBTYPE_ARM64_ALL
        bytes::u32(out, header.filetype);
        bytes::u32(out, field("mach_header_64.ncmds", self.count() as u64)?);
        bytes::u32(out, field("mach_header_64.sizeofcmds", size)?);
        bytes::u32(out, header.flags);
        bytes::u32(out, 0); // reserved
        let mut uuid_at = None;
        for item in self.items.iter() {
            match *item {
                Item::Segment { segment, first, count } => {
                    bytes::u32(out, macho::LC_SEGMENT_64);
                    bytes::u32(out, field("segment_command_64.cmdsize", segment_size(count as u64))?);
                    name16(out, segment.name, "segment_command_64.segname")?;
                    bytes::u64(out, segment.vmaddr);
                    bytes::u64(out, segment.vm_size);
                    bytes::u64(out, segment.file);
                    bytes::u64(out, segment.file_size);
                    bytes::u32(out, segment.prot);
                    bytes::u32(out, segment.prot);
                    bytes::u32(out, field("segment_command_64.nsects", count as u64)?);
                    bytes::u32(out, segment.flags);
                    for s in self.sections[first..first + count].iter() {
                        name16(out, s.name, "section_64.sectname")?;
                        name16(out, s.seg, "section_64.segname")?;
                        bytes::u64(out, s.addr);
                        bytes::u64(out, s.size);
                        bytes::u32(out, field("section_64.offset", s.offset)?);
                        bytes::u32(out, s.align);
                        bytes::u32(out, 0); // reloff
                        bytes::u32(out, 0); // nreloc
                        bytes::u32(out, s.flags);
                        bytes::u32(out, s.reserved1);
                        bytes::u32(out, 0); // reserved2
                        bytes::u32(out, 0); // reserved3
                    }
                }
                Item::Other(command) => {
                    let cmdsize = command.size();
                    match command {
                        Command::LinkeditData { cmd, off, size } => {
                            bytes::u32(out, cmd);
                            bytes::u32(out, field("linkedit_data_command.cmdsize", cmdsize)?);
                            bytes::u32(out, field("linkedit_data_command.dataoff", off)?);
                            bytes::u32(out, field("linkedit_data_command.datasize", size)?);
                        }
                        Command::Symtab { symoff, nsyms, stroff, strsize } => {
                            bytes::u32(out, macho::LC_SYMTAB);
                            bytes::u32(out, field("symtab_command.cmdsize", cmdsize)?);
                            bytes::u32(out, field("symtab_command.symoff", symoff)?);
                            bytes::u32(out, field("symtab_command.nsyms", nsyms)?);
                            bytes::u32(out, field("symtab_command.stroff", stroff)?);
                            bytes::u32(out, field("symtab_command.strsize", strsize)?);
                        }
                        Command::Dysymtab { fields } => {
                            bytes::u32(out, macho::LC_DYSYMTAB);
                            bytes::u32(out, field("dysymtab_command.cmdsize", cmdsize)?);
                            for value in fields {
                                bytes::u32(out, field("a dysymtab_command field", value)?);
                            }
                        }
                        Command::Dylinker { name } => {
                            bytes::u32(out, macho::LC_LOAD_DYLINKER);
                            bytes::u32(out, field("dylinker_command.cmdsize", cmdsize)?);
                            bytes::u32(out, DYLINKER_NAME_AT as u32);
                            padded(out, name, cmdsize - DYLINKER_NAME_AT);
                        }
                        Command::Uuid => {
                            bytes::u32(out, macho::LC_UUID);
                            bytes::u32(out, field("uuid_command.cmdsize", cmdsize)?);
                            uuid_at = Some(out.len());
                            out.extend_from_slice(&[0u8; UUID_LEN]);
                        }
                        Command::BuildVersion { platform, minos, sdk } => {
                            bytes::u32(out, macho::LC_BUILD_VERSION);
                            bytes::u32(out, field("build_version_command.cmdsize", cmdsize)?);
                            bytes::u32(out, platform);
                            bytes::u32(out, minos);
                            bytes::u32(out, sdk);
                            bytes::u32(out, 0); // ntools
                        }
                        Command::Main { entry, stack } => {
                            bytes::u32(out, macho::LC_MAIN);
                            bytes::u32(out, field("entry_point_command.cmdsize", cmdsize)?);
                            bytes::u64(out, entry);
                            bytes::u64(out, stack);
                        }
                        Command::Dylib { cmd, name, timestamp, current, compat } => {
                            bytes::u32(out, cmd);
                            bytes::u32(out, field("dylib_command.cmdsize", cmdsize)?);
                            bytes::u32(out, DYLIB_NAME_AT as u32);
                            bytes::u32(out, timestamp);
                            bytes::u32(out, current);
                            bytes::u32(out, compat);
                            padded(out, name, cmdsize - DYLIB_NAME_AT);
                        }
                    }
                }
            }
        }
        let uuid_at = uuid_at.ok_or(CommandsError::Field { what: "LC_UUID (the list has none)", value: 0 })?;
        Ok(Written { uuid_at })
    }
}

/// The list's bytes walk back by their `cmdsize`s to exactly `ncmds` commands ending at
/// `sizeofcmds`, each `cmdsize` the command's own size; the room is refused by the typed error,
/// one byte short of the need, in every build.
#[cfg(test)]
mod tests {
    use blaze_own::{AVec, Alloc, Vec};

    use super::{Command, Commands, CommandsError, HEADER, Header, Section, Segment, segment_size};
    use crate::bytes;
    use crate::macho;

    fn list<'n>(install: &'n str, alloc: Alloc<'static>) -> Commands<'n, 'static> {
        let mut cmds = Commands::new(alloc);
        let text = Segment { name: "__TEXT", vmaddr: 0, vm_size: 0x4000, file: 0, file_size: 0x4000, prot: 5, flags: 0 };
        let section = Section { name: "__text", seg: "__TEXT", addr: 0x4000, size: 4, offset: 0x4000, align: 2, flags: 0, reserved1: 0 };
        cmds.segment(text, [section, Section { name: "__const", ..section }]);
        cmds.push(Command::Dylib { cmd: macho::LC_LOAD_DYLIB, name: install, timestamp: 2, current: 1 << 16, compat: 1 << 16 });
        cmds.push(Command::Uuid);
        cmds.push(Command::Dylinker { name: "/usr/lib/dyld" });
        cmds.push(Command::LinkeditData { cmd: macho::LC_CODE_SIGNATURE, off: 0x8000, size: 0x100 });
        cmds
    }

    #[test]
    fn the_commands_walk_back_by_cmdsize_and_a_short_room_is_refused_by_type() {
        let alloc = Alloc::global();
        let install = "x".repeat(300);
        let cmds = list(&install, alloc);
        let need = HEADER + cmds.size();
        let mut out: AVec<'static, u8> = Vec::new_in(alloc);
        let written = cmds.write(&mut out, Header { filetype: 6, flags: 0 }, need).unwrap();
        assert_eq!(out.len() as u64, need, "the bytes are the list's size");
        let (ncmds, sizeofcmds) = (bytes::get32(&out, 16), u64::from(bytes::get32(&out, 20)));
        assert_eq!((ncmds, sizeofcmds), (5, cmds.size()));
        let mut at = HEADER as usize;
        let mut sizes = [0u64; 5];
        for size in sizes.iter_mut() {
            *size = u64::from(bytes::get32(&out, at + 4));
            assert!(*size > 0 && size.is_multiple_of(8), "cmdsize {size} at {at:#x}");
            at += *size as usize;
        }
        assert_eq!(at as u64, HEADER + sizeofcmds, "ncmds commands end at sizeofcmds");
        assert_eq!(sizes, [segment_size(2), 24 + 304, 24, 32, 16]);
        assert_eq!(written.uuid_at, HEADER as usize + segment_size(2) as usize + 328 + 8);
        let name_at = HEADER as usize + segment_size(2) as usize + 24;
        assert_eq!(&out[name_at..name_at + 300], install.as_bytes());
        assert_eq!(out[name_at + 300], 0, "the install name is NUL-terminated");
        // Twin: one byte less room is refused, naming both sizes.
        let mut short: AVec<'static, u8> = Vec::new_in(alloc);
        assert_eq!(cmds.write(&mut short, Header { filetype: 6, flags: 0 }, need - 1), Err(CommandsError::Room { need, room: need - 1 }));
        // Control: a 17-byte segment name is no segname, refused by its field.
        let mut long = Commands::new(alloc);
        long.segment(Segment { name: "__SEVENTEEN_BYTES", vmaddr: 0, vm_size: 0, file: 0, file_size: 0, prot: 0, flags: 0 }, []);
        long.push(Command::Uuid);
        let mut bad: AVec<'static, u8> = Vec::new_in(alloc);
        assert_eq!(long.write(&mut bad, Header { filetype: 6, flags: 0 }, 0x4000), Err(CommandsError::Field { what: "segment_command_64.segname", value: 17 }));
    }

    /// The executable's room is its writer's list at its largest (macho.rs `command_room`):
    /// with no native image, five segments at their most sections (1080 bytes: `__DATA_CONST`
    /// keeps room for `__got`, `__mod_init_func`, and `__const`) and the ten commands after them
    /// (312), the code at 0x590; a native image adds its two segments at their most sections and
    /// one `dylib_command` per library, by that library's name.
    #[test]
    fn the_executables_room_is_its_commands_at_their_largest() {
        let none = macho::command_room(None);
        assert_eq!(none, HEADER + 1080 + 312);
        assert_eq!(macho::plan().text_at, 0x590);
        let mut install: blaze_own::AString<'static> = blaze_own::String::new_in(Alloc::global());
        install.push_str(&"/".repeat(300));
        let libraries = [install];
        let with = macho::command_room(Some(&libraries[..]));
        assert_eq!(with - none, segment_size(2) + segment_size(8) + 24 + 304);
    }
}
