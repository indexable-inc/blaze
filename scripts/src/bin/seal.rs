//! seal: the publisher of blaze's sealed layers (crates/blaze-cargo/src/serve/sealed.rs), one
//! pack for each tree the toolchain ships (`blaze store cover`: the library, and bstd when the
//! release ships it), dependencies first. Each tree is sealed in its own fresh `BLAZE_HOME`,
//! all of them on one fresh machine directory (`XDG_CACHE_HOME`, where blaze keeps what a
//! machine's homes share, crates/blaze-cargo/src/serve.rs `machine`):
//!
//! 1. after the first tree, `blaze store install --from <out>`: the layers of the trees before
//!    it, from the packs this run wrote, so that their records are read from those layers and
//!    not built (and exported) again; it also places this tree's sources from the release the
//!    blaze is (`toolchain::library::place`), so seal runs on a published release's blaze;
//! 2. the tree's probes built with `--no-prebuilt` (nothing taken from a source, not even the
//!    engine's own `share/blaze/sealed`), in each profile, by `blaze run`, `blaze build` and
//!    `blaze test`: the library's are `probes`; bstd's is the package `blaze new --bstd`
//!    writes on the placed tree, as a user's would be;
//! 3. every record that home's store holds exported as one pack under the tree's cover pin,
//!    signed when a key is given, into `out` in blaze-pack's layout, then `blaze gc` and
//!    `blaze stop`.
//!
//! With `upload`, a separate last step copies `out` to an S3-compatible bucket (Cloudflare R2's
//! endpoint is one) with the `aws` program, credentials from its environment, never printed.
//!
//!     blaze run -q -p scripts --bin seal -- '{"blaze": "/opt/blaze/blaze", "probes": ["/tmp/hello"], "out": "/tmp/sealed", "sign_key": "/k/seal.key", "public_key": "<64 hex>"}'
//!
//! The input is one JSON object:
//!
//!     blaze       the blaze that builds and exports: a release's, whose shipped trees are sealed
//!     probes      project directories whose builds reach the library's records
//!     profiles    the profiles each probe is built in (["dev", "release"])
//!     out         the directory the packs are written to, which must not exist or be empty:
//!                 the base a `packs.json` names
//!     sign_key    a file holding the Ed25519 secret the heads are signed with, or null
//!     public_key  that secret's public key (64 hex digits), or null: the key the homes after
//!                 the first trust, since an install takes only a signed head. More than one
//!                 shipped tree needs both
//!     upload      null, or {"endpoint": "https://<account>.r2.cloudflarestorage.com",
//!                 "bucket": "...", "prefix": "..."}: `out` copied under `s3://bucket/prefix`
//!
//! Nothing is uploaded unless `upload` says so. A step that fails stops the script, naming it.

use std::path::{Path, PathBuf};
use std::process::Command;

use blaze_script::{
    fail,
    input::{self, Fields, Input, InputError},
};

const SCRIPT: &str = "seal";

/// Where `out` is copied to.
struct Upload {
    endpoint: String,
    bucket: String,
    prefix: String,
}

impl Input for Upload {
    const FIELDS: &'static [&'static str] = &["endpoint", "bucket", "prefix"];

    fn parse(f: &Fields<'_>) -> Result<Upload, InputError> {
        Ok(Upload { endpoint: f.str("endpoint", None)?, bucket: f.str("bucket", None)?, prefix: f.str("prefix", Some(""))? })
    }
}

/// The input, frozen once parsed.
struct Args {
    blaze: String,
    probes: Vec<String>,
    profiles: Vec<String>,
    out: String,
    sign_key: Option<String>,
    public_key: Option<String>,
    upload: Option<Upload>,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["blaze", "probes", "profiles", "out", "sign_key", "public_key", "upload"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        let profiles = f.strs("profiles")?;
        Ok(Args {
            blaze: f.str("blaze", None)?,
            probes: f.strs_required("probes")?,
            profiles: if profiles.is_empty() { vec!["dev".to_owned(), "release".to_owned()] } else { profiles },
            out: f.str("out", None)?,
            sign_key: f.str_or_null("sign_key")?,
            public_key: f.str_or_null("public_key")?,
            upload: f.object("upload", false)?,
        })
    }
}

/// The directories one tree's commands run with: its own home, and the machine directory every
/// tree of this run shares (where the layers are installed and the trees placed).
struct Home {
    home: PathBuf,
    cache: PathBuf,
}

impl Home {
    /// `program args` from `dir` with this home and machine: its stdout, or the script stops
    /// naming the command and what it said.
    fn ran(&self, program: &Path, args: &[&str], dir: &Path) -> String {
        let said = format!("{} {}", program.display(), args.join(" "));
        let out = Command::new(program)
            .args(args)
            .env("BLAZE_HOME", &self.home)
            .env("XDG_CACHE_HOME", &self.cache)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|error| fail(SCRIPT, &format!("{said}: {error}")));
        if !out.status.success() {
            fail(SCRIPT, &format!("{said} in {}: {}\n{}", dir.display(), out.status, String::from_utf8_lossy(&out.stderr)));
        }
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

/// One shipped tree as `blaze store cover` names it: `<pin> <dir>: <whether installed>`.
struct Tree {
    pin: String,
    dir: PathBuf,
}

/// The shipped trees of `cover`'s output, in its order (dependencies first). A line that is not
/// `<pin> <dir>: ...` stops the script, naming it.
fn trees(cover: &str) -> Vec<Tree> {
    cover
        .lines()
        .map(|line| {
            let parsed = line.split_once(' ').and_then(|(pin, rest)| Some((pin, rest.split_once(": ")?.0)));
            match parsed {
                Some((pin, dir)) if pin.starts_with("sealed-") => Tree { pin: pin.to_owned(), dir: PathBuf::from(dir) },
                _ => fail(SCRIPT, &format!("`blaze store cover` said `{line}`, not `sealed-<hex> <dir>: ...`")),
            }
        })
        .collect()
}

fn made(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap_or_else(|error| fail(SCRIPT, &format!("{}: {error}", dir.display())));
}

fn main_with(it: &Args) {
    let blaze = std::fs::canonicalize(&it.blaze).unwrap_or_else(|error| fail(SCRIPT, &format!("{}: {error}", it.blaze)));
    let out = std::path::absolute(&it.out).unwrap_or_else(|error| fail(SCRIPT, &format!("{}: {error}", it.out)));
    // A pack of a tree already in `out` would be installed as the layer that tree is sealed on,
    // and its records then neither built nor exported.
    if std::fs::read_dir(&out).is_ok_and(|mut entries| entries.next().is_some()) {
        fail(SCRIPT, &format!("{} has files: the packs are written into a directory that is empty or not there", out.display()));
    }
    made(&out);
    let out_text = out.to_string_lossy().into_owned();
    let work = std::env::temp_dir().join(format!("blaze-seal-{}", std::process::id()));
    if work.exists() {
        fail(SCRIPT, &format!("{} is there already: the homes and the machine must be fresh", work.display()));
    }
    let cache = work.join("cache");
    made(&cache);
    let home_of = |at: usize| {
        let home = Home { home: work.join(format!("home-{at}")), cache: cache.clone() }; // clone: once a tree
        made(&home.home);
        home
    };
    let first = home_of(0);
    let trees = trees(&first.ran(&blaze, &["store", "cover"], &work));
    if trees.is_empty() {
        fail(SCRIPT, "`blaze store cover` names no shipped tree");
    }
    let is_key = |key: &str| key.len() == 64 && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let public_key = match (&it.sign_key, &it.public_key) {
        (_, Some(key)) if !is_key(key) => fail(SCRIPT, &format!("`public_key` is `{key}`, not 64 lower-case hex digits")),
        (None, Some(_)) => fail(SCRIPT, "`public_key` is given without the `sign_key` it is the key of"),
        (Some(_), Some(key)) => Some(key.as_str()),
        // Said before anything is built; the tree's own line below says it again by name.
        (_, None) if trees.len() > 1 => fail(
            SCRIPT,
            &format!("`blaze store cover` names {} shipped trees, and those after the first need `sign_key` and its `public_key`", trees.len()),
        ),
        (_, None) => None,
    };
    for (at, tree) in trees.iter().enumerate() {
        let later;
        let home = match at {
            0 => &first,
            _ => {
                later = home_of(at);
                &later
            }
        };
        if at > 0 {
            let Some(key) = public_key else {
                fail(
                    SCRIPT,
                    &format!(
                        "`blaze store cover` names {} shipped trees: {} is sealed on the layers of those before it, installed from `out`, and an install takes only a signed head: give `sign_key` and its `public_key`",
                        trees.len(),
                        tree.pin
                    ),
                )
            };
            let settings = serde_json::json!({ "trusted": [key] }).to_string();
            let path = home.home.join("packs.json");
            std::fs::write(&path, settings).unwrap_or_else(|error| fail(SCRIPT, &format!("{}: {error}", path.display())));
            print!("{}", home.ran(&blaze, &["store", "install", "--from", out_text.as_str()], &work));
        }
        let probes: Vec<PathBuf> = match tree.dir.parent().and_then(Path::file_name).and_then(|it| it.to_str()) {
            _ if at == 0 => it.probes.iter().map(PathBuf::from).collect(),
            Some("bstd") => {
                let probe = work.join(format!("probe-{at}"));
                let bstd = tree.dir.join("bstd").join("crates").join("blaze-std");
                home.ran(&blaze, &["new", "--vcs", "none", "--bstd", &bstd.to_string_lossy(), &probe.to_string_lossy()], &work);
                vec![probe]
            }
            _ => fail(SCRIPT, &format!("seal has no probe for the shipped tree {} at {}", tree.pin, tree.dir.display())),
        };
        for dir in &probes {
            for profile in &it.profiles {
                for command in ["run", "build", "test"] {
                    home.ran(&blaze, &[command, "--no-prebuilt", "--profile", profile], dir);
                }
            }
        }
        // Absolute: the commands run from `work`, where a path relative to here names nothing.
        let key = it.sign_key.as_ref().map(|key| {
            std::path::absolute(key).unwrap_or_else(|error| fail(SCRIPT, &format!("{key}: {error}"))).to_string_lossy().into_owned()
        });
        let mut export = vec!["store", "export", "--all", "--pin", tree.pin.as_str(), "--out", out_text.as_str()];
        if let Some(key) = &key {
            export.extend(["--sign-key", key.as_str()]);
        }
        print!("{}", home.ran(&blaze, &export, &work));
        print!("{}", home.ran(&blaze, &["gc"], &work));
        home.ran(&blaze, &["stop"], &work);
    }
    if let Some(upload) = &it.upload {
        // Credentials are the `aws` program's own environment (AWS_ACCESS_KEY_ID,
        // AWS_SECRET_ACCESS_KEY): read by it, never by this script, never printed.
        let to = format!("s3://{}/{}", upload.bucket, upload.prefix.trim_matches('/'));
        let aws = PathBuf::from("aws");
        first.ran(&aws, &["s3", "sync", out_text.as_str(), &to, "--endpoint-url", &upload.endpoint, "--only-show-errors"], &work);
        println!("uploaded {} to {to} at {}", out.display(), upload.endpoint);
    }
    _ = std::fs::remove_dir_all(&work);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] => main_with(&input::parse::<Args>(one, SCRIPT)),
        _ => fail(SCRIPT, "seal takes one JSON value: see the top of scripts/src/bin/seal.rs"),
    }
}
