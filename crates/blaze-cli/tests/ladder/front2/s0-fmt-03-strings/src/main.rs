//! s0-fmt-03: strings and chars through `std`'s formatting.
//!
//! The `std::fmt` docs: strings are left-aligned by default; `.N` on a string truncates it to N
//! chars; padding counts `char`s (`Formatter::pad` uses `s.chars().count()`); `{{` and `}}` are
//! literal braces. `Debug` for `str` escapes with `char::escape_debug_ext` (quote, backslash,
//! `\n`, `\t`, and non-printable chars as `\u{..}`; `é` is printable and kept); `Debug` for
//! `char` escapes a single quote. `str::len` is the UTF-8 byte length.

fn main() {
    let s = "abc";
    println!("[{:>6}]", s);
    println!("[{:6}]", s);
    println!("[{:-<6}]", s);
    println!("[{:*^7}]", s);
    println!("[{:*^8}]", s);
    println!("[{:.2}]", "abcdef");
    println!("[{:>5.2}]", "abcdef");
    println!("[{:>7}]", "héllo");
    println!("{:?}", "a\"b\n\tc");
    println!("{:?} {:?}", 'x', '\'');
    println!("{}", "über".len());
    println!("{}", "über".chars().count());
    let owned = String::from("own");
    println!("{owned}-{}", owned.len());
    println!("{0}{1}{0}", "a", "b");
    println!("{{}} {:?}", "");
    println!("{:?}", "é\u{7f}");
}
