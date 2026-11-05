//@ edition: 2021
pub struct S;
pub fn broken() -> u32 { S.missing() }
