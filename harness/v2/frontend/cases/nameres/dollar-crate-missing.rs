//@ expect: reject E0425
//@ edition: 2021
macro_rules! call {
    () => {
        $crate::nope()
    };
}

fn main() {
    call!();
}
