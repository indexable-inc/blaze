//@ expect: reject E0308
const NEVER_READ: u32 = "const"; //~ ERROR E0308

fn main() {}
