//! The boot the files are read in, asked of the files capability (D104 B1 piece 2,
//! port27/d104-b1-entries.md): a pack's bytes known whole in this boot may be mapped unread
//! (blaze-cli store.rs `Boot`), so the fact belongs to whoever holds the files, and a command
//! that names `fs: Fs` needs no whole `Io` to ask it. It is the same fact `Io::boot` gives: the
//! driver's, read once at its start (blaze-core `Driver::boot`).

use super::Fs;

impl Fs {
    /// The boot this world's driver runs in: equal within one boot, different across two; a
    /// hosted guest has none (its host table carries no boot) and says so.
    pub fn boot(&self) -> Result<blaze_core::Boot, blaze_core::NoBoot> {
        self.io.boot()
    }
}

#[cfg(test)]
mod tests {
    use crate::Io;

    /// The files capability gives its world's boot fact, the one `Io::boot` gives (the
    /// simulation's boot, drawn from its seed and drawn again by a crash, which "comes back as
    /// another boot", blaze-core sim.rs `crash_with`). A body that made up its own answer (a
    /// `NoBoot`, a constant boot) fails the second half: the boot after the crash is followed.
    /// Passes as `fs::boot::tests::the_files_capability_gives_its_worlds_boot ... ok`.
    #[test]
    fn the_files_capability_gives_its_worlds_boot() {
        let io = Io::sim(1);
        let fs = io.need_fs().expect("a simulated world offers its files");
        let before = fs.boot().expect("the simulation has a boot");
        assert_eq!(Ok(before), io.boot());
        assert!(io.crash(), "a simulated world crashes");
        let after = fs.boot().expect("the simulation has a boot after a crash");
        assert_eq!(Ok(after), io.boot());
        assert_ne!(before, after, "a crash is another boot, and the files capability says so");
    }
}
