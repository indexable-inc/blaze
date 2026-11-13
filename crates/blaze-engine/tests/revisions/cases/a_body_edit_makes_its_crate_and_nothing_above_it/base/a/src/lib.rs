pub fn base(x: u32) -> u32 {
    x + 1
}

pub fn beside(x: u32) -> u32 {
    x * 5
}

pub fn twice<T: Copy + core::ops::Add<Output = T>>(t: T) -> T {
    t + t
}
