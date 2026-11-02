//@ expect: reject "cannot find macro"
//@ edition: 2021
mod m {
    macro_rules! inner {
        () => {};
    }
    pub fn use_it() {
        inner!();
    }
}

fn main() {
    m::use_it();
    inner!();
}
