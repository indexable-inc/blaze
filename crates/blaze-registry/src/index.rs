//! cargo's cache of an index file (`src/sources/registry/index/cache.rs`), which blaze reads and
//! writes in cargo's format, so that the two share `$CARGO_HOME/registry/index`. The lines are
//! read as blaze-units' [`blaze_units::index`] reads them (cargo 1.99's `IndexSummary::parse`).

use std::ops::Range;

use anyhow::{Context, bail};
use semver::{Version, VersionReq};

pub use blaze_units::index::{
    Artifact, INDEX_V_MAX, IndexDependency, IndexEntry, IndexLine, Status, dep_path, index_path,
    line_version, parse_line,
};

/// The version of the format of cargo's cache of an index file.
const CACHE_VERSION: u8 = 3;

/// The versions of one package an index file has, each with its line, as cargo's
/// `Summaries`: from the file itself, or from cargo's cache of it.
#[derive(Clone, Debug, Default)]
pub struct Summaries {
    raw: Vec<u8>,
    versions: Vec<(Version, Range<usize>)>,
}

impl Summaries {
    /// The lines of an index file whose name and version can be read. Lines are split at `\n`
    /// only, as cargo splits them.
    pub fn from_index_file(raw: Vec<u8>) -> Summaries {
        let mut versions = Vec::new();
        let mut at = 0;
        while at < raw.len() {
            let end = raw[at..].iter().position(|&it| it == b'\n').map_or(raw.len(), |it| at + it);
            if let Some(version) = line_version(&raw[at..end]) {
                versions.push((version, at..end));
            }
            at = end + 1;
        }
        Summaries { raw, versions }
    }

    /// A cache file cargo or blaze wrote, and the version of the index file it was made from.
    pub fn from_cache(contents: Vec<u8>) -> anyhow::Result<(Summaries, String)> {
        let (&first, rest) = contents.split_first().context("malformed cache")?;
        if first != CACHE_VERSION {
            bail!("looks like a different Cargo's cache, bailing out");
        }
        let schema = rest.get(..4).context("cache expected 4 bytes for index schema version")?;
        let schema = u32::from_le_bytes(schema.try_into().expect("four bytes"));
        if schema != INDEX_V_MAX {
            bail!("index schema version {schema} doesn't match the version I know ({INDEX_V_MAX})");
        }
        let mut parts = Split { data: &contents, at: 5 };
        let index_version = parts.next().context("malformed file")?;
        let index_version = std::str::from_utf8(&contents[index_version])?.to_owned();
        let mut versions = Vec::new();
        while let Some(version) = parts.next() {
            let version = Version::parse(std::str::from_utf8(&contents[version])?)?;
            let line = parts.next().context("a version in the cache has no line")?;
            versions.push((version, line));
        }
        Ok((Summaries { raw: contents, versions }, index_version))
    }

    /// The cache of these versions, for an index file of `index_version`, as cargo writes it.
    pub fn to_cache(&self, index_version: &str) -> Vec<u8> {
        let mut out = vec![CACHE_VERSION];
        out.extend(INDEX_V_MAX.to_le_bytes());
        out.extend(index_version.as_bytes());
        out.push(0);
        for (version, line) in &self.versions {
            out.extend(version.to_string().as_bytes());
            out.push(0);
            out.extend(&self.raw[line.clone()]);
            out.push(0);
        }
        out
    }

    pub fn len(&self) -> usize {
        self.versions.len()
    }

    /// Each version and its line, in the order of the file.
    pub fn lines(&self) -> impl Iterator<Item = (&Version, &[u8])> {
        self.versions.iter().map(|(version, line)| (version, &self.raw[line.clone()]))
    }

    /// The versions that match `req`, read; one whose line is not what it was when its version
    /// was read is left out, as cargo leaves it out.
    pub fn matching<'a>(&'a self, req: &'a VersionReq) -> impl Iterator<Item = IndexLine> + 'a {
        self.select(|version| req.matches(version))
    }

    /// The versions `keep` keeps, read as [`Summaries::matching`] reads them.
    pub fn select<'a>(
        &'a self,
        mut keep: impl FnMut(&Version) -> bool + 'a,
    ) -> impl Iterator<Item = IndexLine> + 'a {
        self.lines()
            .filter(move |(version, _)| keep(version))
            .filter_map(|(_, line)| parse_line(line).ok())
    }

    /// Every version, read.
    pub fn all(&self) -> impl Iterator<Item = IndexLine> + '_ {
        self.lines().filter_map(|(_, line)| parse_line(line).ok())
    }
}

/// The parts of a cache file between its zeros, as ranges of it.
struct Split<'a> {
    data: &'a [u8],
    at: usize,
}

impl Iterator for Split<'_> {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Range<usize>> {
        if self.at >= self.data.len() {
            return None;
        }
        let start = self.at;
        let end = self.data[start..]
            .iter()
            .position(|&it| it == 0)
            .map_or(self.data.len(), |it| start + it);
        self.at = end + 1;
        Some(start..end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = concat!(
        r#"{"name":"a","vers":"1.0.0","deps":[],"cksum":"00"}"#,
        "\n\n",
        r#"{"name":"a","vers":"1.0.1+meta","deps":[],"cksum":"01","yanked":true}"#,
        "\nnot json\n",
        r#"{"name":"a","vers":"2.0.0","deps":[{"name":"b","req":"x"}],"cksum":"02"}"#,
        "\n",
        r#"{"vers":"3.0.0"}"#,
        "\n",
        r#"{"name":"a","vers":"one"}"#,
        "\n",
        r#"{"name":"a","vers":"4.0.0-rc.1","deps":"not a list"}"#,
    );

    #[test]
    fn cache_round_trip() {
        let file = concat!(
            r#"{"name":"a","vers":"1.0.0","deps":[],"cksum":"00"}"#,
            "\n\n",
            r#"{"name":"a","vers":"1.0.1+meta","deps":[],"cksum":"01","yanked":true}"#,
            "\nnot json\n",
            r#"{"name":"a","vers":"2.0.0","deps":[{"name":"b","req":"x"}],"cksum":"02"}"#,
            "\n",
        );
        let summaries = Summaries::from_index_file(file.as_bytes().to_vec());
        let versions: Vec<String> = summaries.lines().map(|(it, _)| it.to_string()).collect();
        assert_eq!(versions, ["1.0.0", "1.0.1+meta", "2.0.0"]);
        let cache = summaries.to_cache("etag: \"abc\"");
        assert_eq!(&cache[..5], &[3, 2, 0, 0, 0]);
        let (read, index_version) = Summaries::from_cache(cache.clone()).unwrap();
        assert_eq!(index_version, "etag: \"abc\"");
        assert_eq!(read.to_cache("etag: \"abc\""), cache);
        let req = VersionReq::parse("^1").unwrap();
        let statuses: Vec<Status> = read.matching(&req).map(|it| it.status).collect();
        assert_eq!(statuses, [Status::Candidate, Status::Yanked]);
        assert!(Summaries::from_cache(vec![2, 2, 0, 0, 0]).is_err());
        assert!(Summaries::from_cache(vec![3, 3, 0, 0, 0, b'x', 0]).is_err());
    }

    /// v1's `from_index_file`, which parsed each whole line to find its version, kept to check
    /// the version-only probe keeps the same lines at the same versions and spans.
    fn old_from_index_file(raw: &[u8]) -> Vec<(Version, Range<usize>)> {
        let mut versions = Vec::new();
        let mut at = 0;
        while at < raw.len() {
            let end = raw[at..].iter().position(|&it| it == b'\n').map_or(raw.len(), |it| at + it);
            if let Ok(line) = parse_line(&raw[at..end]) {
                versions.push((line.entry.version, at..end));
            }
            at = end + 1;
        }
        versions
    }

    #[test]
    fn a_file_s_versions_are_the_ones_whole_lines_give() {
        for file in [FILE, "", "\n", "\n\n{\"name\":\"a\",\"vers\":\"1.0.0\"}", "{\"name\":\"a\",\"vers\":\"1.0.0\"}\n"] {
            let new = Summaries::from_index_file(file.as_bytes().to_vec());
            assert_eq!(new.versions, old_from_index_file(file.as_bytes()), "{file:?}");
        }
    }
}
