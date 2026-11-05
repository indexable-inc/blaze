//@ expect: reject E0425
//@ edition: 2021
macro_rules! bind {
    () => {
        let hidden = 1;
    };
}

fn main() {
    bind!();
    let _ = hidden;
}
