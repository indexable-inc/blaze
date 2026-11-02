//@ expect: reject E0603
//@ edition: 2021
mod util {
    fn helper() -> u32 {
        1
    }
}

macro_rules! call {
    () => {
        $crate::util::helper()
    };
}

fn main() {
    let _ = call!();
}
