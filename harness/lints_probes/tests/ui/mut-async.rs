//@ edition: 2021
#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub async fn push(mut v: Vec<u8>) -> Vec<u8> { v.push(1); v }
pub async fn len(mut v: Vec<u8>) -> usize { v.len() }
pub async fn local() -> u8 {
    let mut a = 1u8;
    a += 1;
    let mut b = 2u8;
    ready().await;
    a + b
}
pub fn block() -> impl std::future::Future<Output = u8> {
    async {
        let mut c = 1u8;
        c *= 2;
        let mut d = 1u8;
        c + d
    }
}
pub fn closure() -> impl Fn(u8) -> u8 { |mut x| x }
pub fn closure_mut() -> impl Fn(u8) -> u8 { |mut x| { x += 1; x } }
async fn ready() {}
