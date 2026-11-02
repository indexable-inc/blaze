//@ expect: reject E0308
fn main() {
    let flag = true;
    let _x = if flag { 1u32 } else { "one" }; //~ ERROR E0308
}
