//@ expect: reject E0063
//@ compile-flags: --test
//@ edition: 2021
//@ issue: #1522
// rustc rejects a struct literal that leaves out a field inside a `#[test]` function (E0063).
// blaze let it through to codegen, which refused it as `missing field in record literal`.

pub struct Config {
    pub jobs: u32,
    pub told: bool,
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn defaults() {
        let config = Config { jobs: 1 };
        assert_eq!(config.jobs, 1);
    }
}
