//@ expect: reject E0277
struct Token;

fn copy_of<T: Copy>(t: T) -> T {
    t
}

fn main() {
    let slot = Default::default();
    let _a = copy_of(slot); //~ ERROR E0277
    let _t: Token = slot;
}

impl Default for Token {
    fn default() -> Self {
        Token
    }
}
