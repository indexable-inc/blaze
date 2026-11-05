//@ expect: reject E0425
//@ edition: 2021
mod m {
    pub fn here() {}
}

fn main() {
    m::here();
    m::gone();
}
