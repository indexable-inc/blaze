//@ edition: 2021
#[derive(Debug, Default, Clone, PartialEq, PartialOrd)]
pub struct Config { pub name: String, pub level: u8, pub ratio: f32 }
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank { Low, Mid, High }
pub fn default_config() -> Config { Config { level: 2, ..Default::default() } }
