
// What each test executed, for a binary built without jump-table slots: the harness text of
// `blaze_usercode::test::fork_server(false)`, after `fork_server.rs`. Nothing is mapped or
// armed, and every end says the record is not whole and why.
#[cfg(unix)]
mod exec {
    use super::{End, UNRECORDED};
    use ::std::borrow::Cow;

    pub struct Exec;

    pub struct Region;

    impl Exec {
        pub fn open() -> Exec {
            Exec
        }

        pub fn not_run<'a>(&self, _why: &'a str) -> &'a str {
            UNRECORDED
        }

        pub fn map(&mut self) -> Result<Region, String> {
            Ok(Region)
        }

        pub fn give(&mut self, _region: Region, _hits: &[u8], _whole: bool) {}
    }

    impl Region {
        pub fn arm(&self) -> i32 {
            0
        }

        pub fn read(&self, _end: &End, hits: &mut Vec<u8>) -> Result<u32, Cow<'static, str>> {
            hits.clear();
            Err(Cow::Borrowed(UNRECORDED))
        }
    }
}
