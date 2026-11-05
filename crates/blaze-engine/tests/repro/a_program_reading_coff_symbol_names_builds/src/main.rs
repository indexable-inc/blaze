use object::read::coff::{CoffFile, ImageSymbol as _};

fn main() {
    let bytes = std::hint::black_box(&b"short"[..]);
    match CoffFile::<&[u8]>::parse(bytes) {
        Ok(file) => {
            let table = file.coff_symbol_table();
            let name = table.symbol(object::SymbolIndex(0)).ok().and_then(|it| it.name(table.strings()).ok());
            println!("{:?}", name.map(<[u8]>::len));
        }
        Err(error) => println!("{error}"),
    }
}
