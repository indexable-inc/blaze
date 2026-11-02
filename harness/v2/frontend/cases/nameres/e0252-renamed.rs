//@ expect: accept
//@ edition: 2021
use std::fmt::Result;
use std::io::Result as IoResult;

fn fmt_ok() -> Result {
    Ok(())
}

fn io_ok() -> IoResult<u8> {
    Ok(3)
}

fn main() {
    println!("{:?} {:?}", fmt_ok(), io_ok().ok());
}
