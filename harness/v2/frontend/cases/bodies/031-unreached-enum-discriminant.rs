//@ expect: reject E0308
#[repr(u8)]
enum Code {
    Ok = 0,
    Bad = "one", //~ ERROR E0308
}

fn main() {
    let _ = Code::Ok;
    let _ = Code::Bad;
}
