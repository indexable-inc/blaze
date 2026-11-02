//@ expect: accept
//@ edition: 2021
mod mod_file_helper;

use mod_file_helper::deep::Deep;

fn main() {
    println!("{} {}", mod_file_helper::value(), Deep::VALUE);
}
