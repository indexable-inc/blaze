//! `ship-code-identity '{"program": "<dir>", "into": "<dir>"}'`: writes, into the package
//! directory `into` cut from the checkout, what the checkout alone decides of each code identity
//! of the program at `program` (`blaze_codegen_code::Tree::shipped`, one file per identity named
//! by `Identity::shipped`), so that the program's build script, run where there is no checkout
//! (a published crate), works out the identity a build of the checkout would. Run by
//! .github/scripts/publish_crates.py. One JSON value in (AGENTS.md rule 41); a failure names the
//! file and the identity, and exits 1.

use std::path::{Path, PathBuf};

use blaze_codegen_code::{IDENTITIES, Workspace, ship};

/// What it is asked.
struct Request {
    /// The program's package directory, in its checkout.
    program: PathBuf,
    /// The package directory the files are written into: not the program's own.
    into: PathBuf,
}

fn request(text: &str) -> Result<Request, String> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|why| format!("the request `{text}` is no JSON: {why}"))?;
    let object =
        value.as_object().ok_or_else(|| format!("the request `{text}` is no JSON object"))?;
    if let Some(unknown) = object.keys().find(|it| !matches!(it.as_str(), "program" | "into")) {
        return Err(format!(
            "the request has the field `{unknown}`, and takes only `program` and `into`"
        ));
    }
    let path = |name: &str| {
        object
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| format!("the request has no string `{name}`"))
    };
    Ok(Request { program: path("program")?, into: path("into")? })
}

fn shipped(request: &Request) -> Result<Vec<PathBuf>, String> {
    let canonical = |path: &Path| {
        std::fs::canonicalize(path).map_err(|why| format!("{}: {why}", path.display()))
    };
    if canonical(&request.program)? == canonical(&request.into)? {
        return Err(format!(
            "{} is the program's own directory in its checkout: a shipped tree there would stand in \
             for the checkout's files, and go stale with the first edit",
            request.into.display()
        ));
    }
    let workspace = Workspace::read(&request.program)?;
    let mut written = Vec::new();
    for &identity in IDENTITIES {
        let text = ship(&workspace, identity)
            .map_err(|why| format!("{} ({}): {why}", identity.what, identity.env))?;
        let path = request.into.join(identity.shipped);
        std::fs::write(&path, text).map_err(|why| format!("writing {}: {why}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let done = match &args[..] {
        [text] => request(text).and_then(|it| shipped(&it)),
        _ => Err(format!(
            "takes one JSON value, `{{\"program\": DIR, \"into\": DIR}}`, and was given {} arguments",
            args.len()
        )),
    };
    match done {
        Ok(written) => {
            for path in written {
                println!("{}", path.display());
            }
            std::process::ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("error: ship-code-identity: {why}");
            std::process::ExitCode::FAILURE
        }
    }
}
