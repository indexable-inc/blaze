//@ expect: reject E0308
//@ edition: 2021
async fn never_awaited() -> u32 {
    "async" //~ ERROR E0308
}

fn main() {}
