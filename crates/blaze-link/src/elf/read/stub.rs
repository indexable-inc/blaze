//! blaze's own stubs of a C library's shared objects ([`crate::target`]'s `shipped`): what an
//! image may import from one, as text. The first line is [`STUB`]; then `soname NAME`; then a
//! line per symbol, `KIND VERSION NAME`, with `F` a function, `D` data and `T` a thread-local,
//! each at the version the library defines it at by default. It stands where the library's own
//! `.so` would on the link line and is read into the same [`Shared`].

use crate::model::{Name, Names, Shared, SharedSymbol, SymbolKind};
pub(crate) use crate::target::STUB;

pub(super) fn read(name: &str, bytes: &[u8]) -> Result<Shared, String> {
    // alloc: error path
    let text = std::str::from_utf8(bytes).map_err(|_| format!("{name}: a stub that is not UTF-8"))?;
    let mut lines = text[STUB.len()..].lines();
    let soname = lines
        .next()
        .and_then(|it| it.strip_prefix("soname "))
        // alloc: error path
        .ok_or_else(|| format!("{name}: a stub whose second line is not `soname NAME`"))?;
    // The names are the text's, each once.
    let mut names = Names::with_capacity(text.len());
    let count = text.len() / 24;
    let mut symbols = Vec::with_capacity(count);
    // alloc: once per stub, its few versions
    let mut versions: Vec<Name> = Vec::new();
    for line in lines {
        let mut parts = line.splitn(3, ' ');
        let (Some(kind), Some(version), Some(symbol)) = (parts.next(), parts.next(), parts.next())
        else {
            // alloc: error path
            return Err(format!("{name}: a stub line that is not `KIND VERSION NAME`: `{line}`"));
        };
        let kind = match kind {
            "F" => SymbolKind::Function,
            "D" => SymbolKind::Data,
            "T" => SymbolKind::Tls,
            // alloc: error path
            _ => return Err(format!("{name}: a stub line of kind `{kind}`: `{line}`")),
        };
        let full = |it: String| format!("{name}: {it}"); // alloc: error path
        let slot = match versions.iter().position(|it| names.at(*it) == version) {
            Some(slot) => slot,
            None => {
                versions.push(names.push(version).map_err(full)?);
                versions.len() - 1
            }
        };
        // alloc: error path
        let slot = u16::try_from(slot).map_err(|_| format!("{name}: more than 65,536 versions"))?;
        symbols.push(SharedSymbol {
            name: names.push(symbol).map_err(full)?,
            version: Some(slot),
            kind,
            weak: false,
        });
    }
    Ok(Shared {
        // alloc: once per file, `Shared::name`
        name: soname.to_owned(),
        symbols,
        names,
        versions,
        // alloc: empty `Vec`s, which don't allocate
        more: Vec::new(),
        // alloc: empty `Vec`s, which don't allocate
        inputs: Vec::new(),
        // alloc: empty `Vec`s, which don't allocate
        undefined: Vec::new(),
    })
}
