//@ expect: reject E0308
static NEVER_READ: u32 = 'x'; //~ ERROR E0308

fn main() {}
