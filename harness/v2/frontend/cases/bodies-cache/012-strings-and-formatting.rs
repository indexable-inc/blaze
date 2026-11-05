//@ expect: accept
//@ edition: 2021
//@ aux-crate: strings=strings.rs
fn main() {
    println!("{} {:?}", strings::shout("hi"), strings::words(" a  bc d "));
    println!("{} {}", strings::padded(42), strings::precision(1234.5678));
    println!("{} {:?}", strings::reverse_chars("héllo"), strings::count_utf8("héllo"));
}
