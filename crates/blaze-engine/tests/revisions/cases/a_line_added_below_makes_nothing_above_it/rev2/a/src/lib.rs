pub fn base(x: u32) -> u32 {
    let more = 6;
    x + 1 + more
}

pub fn beside(x: u32) -> u32 {
    x * 5
}

pub fn twice<T: Copy + core::ops::Add<Output = T>>(t: T) -> T {
    t + t
}
