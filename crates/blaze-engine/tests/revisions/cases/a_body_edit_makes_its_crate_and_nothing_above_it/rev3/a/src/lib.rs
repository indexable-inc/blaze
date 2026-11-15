pub fn base(x: u64) -> u64 {
    x + 7
}

pub fn beside(x: u32) -> u32 {
    x * 5
}

pub fn twice<T: Copy + core::ops::Add<Output = T>>(t: T) -> T {
    t + t
}
