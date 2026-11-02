//@ expect: reject E0603
//@ edition: 2021
mod m {
    fn hidden() {}
    pub fn shown() {
        hidden();
    }
}

fn main() {
    m::shown();
    m::hidden();
}
