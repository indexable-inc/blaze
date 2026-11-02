//! Shared objects (`ET_DYN`): what an image may import from them.

use object::elf;
use object::read::elf::{Dyn as _, FileHeader as _, Sym as _};

use super::strings::Table;
use super::{ENDIAN, Header};
use crate::model::{Name, Names, Shared, SharedSymbol};

/// Reads the shared object `data`, whose header [`super::read`] checked.
pub(super) fn read(name: &str, data: &[u8]) -> Result<Shared, String> {
    let error = |error: object::read::Error| format!("{name}: {error}"); // alloc: error path
    let header = Header::parse(data).map_err(error)?;
    let sections = header.sections(ENDIAN, data).map_err(error)?;
    let dynsym = sections.symbols(ENDIAN, data, elf::SHT_DYNSYM).map_err(error)?;
    let table = sections.versions(ENDIAN, data).map_err(error)?;

    let mut soname = None;
    if let Some((entries, strings)) = sections.dynamic(ENDIAN, data).map_err(error)? {
        let strings = sections.strings(ENDIAN, data, strings).map_err(error)?;
        for entry in entries {
            if entry.d_tag(ENDIAN) != elf::DT_SONAME {
                continue;
            }
            let bytes = u32::try_from(entry.d_val(ENDIAN))
                .ok()
                .and_then(|offset| strings.get(offset).ok())
                .ok_or_else(|| format!("{name}: DT_SONAME is past the dynamic string table"))?; // alloc: error path
            soname = Some(super::utf8(name, "DT_SONAME", bytes)?.to_owned()); // alloc: once per file, `Shared::name`
        }
    }

    // The names are `.dynstr`'s, each string pushed once: the arena is at most its size.
    let mut dynstr = match dynsym.len() {
        0 => Table::new(name, &[])?,
        _ => super::table(name, &sections, dynsym.string_section(), data)?,
    };
    let mut names = Names::with_capacity(dynstr.size());
    let mut symbols = Vec::with_capacity(dynsym.len());
    // alloc: once per version the library defines, `Shared::versions`; empty allocates nothing
    let mut versions: Vec<Name> = Vec::new();
    // By ELF version index (at most 0x7fff): its slot in `versions` once seen, `None` inside
    // for the local and global indexes, which name no version. Reserved whole, so it never
    // grows: untouched pages past the highest index seen are never written.
    let mut slots: Vec<Option<Option<u16>>> =
        Vec::with_capacity(if table.is_some() { usize::from(elf::VERSYM_VERSION) + 1 } else { 0 });
    for (index, symbol) in dynsym.enumerate() {
        if index.0 == 0 || symbol.st_shndx(ENDIAN) == elf::SHN_UNDEF {
            continue;
        }
        // Read for an error only: the name is pushed once the symbol is known to be kept.
        let symbol_name = || {
            dynsym.symbol_name(ENDIAN, symbol).map_or_else(
                |_| format!("at string offset {}", symbol.st_name(ENDIAN)), // alloc: error path
                |it| String::from_utf8_lossy(it).into_owned(), // alloc: error path
            )
        };
        let weak = match symbol.st_bind() {
            elf::STB_GLOBAL | elf::STB_GNU_UNIQUE => false,
            elf::STB_WEAK => true,
            // A local in `.dynsym` is the library's own: nothing imports it.
            elf::STB_LOCAL => continue,
            other => {
                // alloc: error path
                return Err(format!("{name}: symbol {} has binding {}, which blaze-link does not read", symbol_name(), other.0));
            }
        };
        let version = match &table {
            None => None,
            Some(table) => {
                let versym = table.version_index(ENDIAN, index);
                if versym.is_hidden() {
                    continue;
                }
                let version = versym.index();
                let at = usize::from(version.0);
                if slots.len() <= at {
                    slots.resize(at + 1, None);
                }
                match slots[at] {
                    Some(slot) => slot,
                    None => {
                        let found = table.version(version).map_err(|error| {
                            // alloc: error path
                            format!("{name}: symbol {}: {error}", symbol_name())
                        })?;
                        let slot = match found {
                            None => None,
                            Some(found) => {
                                let found = super::utf8(name, "the version name", found.name())?;
                                // Two indexes may name one version (a definition and a need).
                                Some(match versions.iter().position(|it| names.at(*it) == found) {
                                    Some(slot) => slot as u16,
                                    None => {
                                        let slot = u16::try_from(versions.len()).map_err(|_| {
                                            // alloc: error path
                                            format!("{name}: more than 65,536 versions")
                                        })?;
                                        // alloc: error path
                                        let at = names.push(found).map_err(|it| format!("{name}: {it}"))?;
                                        versions.push(at);
                                        slot
                                    }
                                })
                            }
                        };
                        slots[at] = Some(slot);
                        slot
                    }
                }
            }
        };
        symbols.push(SharedSymbol {
            name: dynstr.name(&mut names, name, "the symbol name", symbol.st_name(ENDIAN))?,
            version,
            kind: super::kind(name, symbol_name, symbol.st_type())?,
            weak,
        });
    }

    Ok(Shared {
        name: soname.unwrap_or_else(|| super::file_name(name)),
        symbols,
        names,
        versions,
        more: Vec::new(),   // alloc: empty, allocates nothing
        inputs: Vec::new(), // alloc: empty, allocates nothing
    })
}
