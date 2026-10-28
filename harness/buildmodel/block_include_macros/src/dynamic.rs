pub fn found() -> u32 {
    answer!() + 1
}

pub mod nested {
    pub fn found() -> u32 {
        answer!() + 2
    }
}
