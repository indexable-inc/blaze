//@ expect: reject E0659
//@ edition: 2021
macro_rules! m {
    () => {};
}

macro_rules! define {
    () => {
        macro_rules! m {
            () => {};
        }
    };
}

define!();

fn main() {
    m!();
}
