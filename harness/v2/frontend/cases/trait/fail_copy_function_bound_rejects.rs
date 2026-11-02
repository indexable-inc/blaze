//@ expect: reject E0277
//@ edition: 2021
// A Copy function bound, failed by a derived-Clone-only struct.
#[derive(Clone)]
struct Only(u8);

fn need_copy<T: Copy>(t: T) -> T {
    t
}

fn main() {
    let _o = need_copy(Only(1));
}
