pub mod deep;

pub fn value() -> u8 {
    deep::Deep::VALUE + super::mod_file_helper::deep::Deep::VALUE
}
