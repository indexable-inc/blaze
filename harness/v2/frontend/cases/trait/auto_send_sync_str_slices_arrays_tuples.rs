//@ expect: accept
//@ edition: 2021
// `str` (through `[u8]`), slices, arrays and tuples take their elements' answer.
fn is_send<T: ?Sized + Send>() -> u32 {
    1
}
fn is_sync<T: ?Sized + Sync>() -> u32 {
    1
}
fn is_unpin<T: ?Sized + Unpin>() -> u32 {
    1
}

fn main() {
    let n = is_send::<str>()
        + is_sync::<str>()
        + is_unpin::<str>()
        + is_send::<[u32]>()
        + is_sync::<[(u8, char)]>()
        + is_send::<[u64; 4]>()
        + is_sync::<[[i8; 2]; 3]>()
        + is_send::<(u8, bool, &'static str)>()
        + is_sync::<((), (u16, [f32; 2]))>()
        + is_unpin::<(u8, [u16; 0])>();
    println!("{n}");
}
