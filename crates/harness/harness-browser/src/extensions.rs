//! The extensions installed, as [`Extension`]s: from the profile's `Default/Preferences` and `Default/Secure
//! Preferences` (`extensions.settings`: id -> location, path, state, disable_reasons, and for some the manifest), each
//! one's manifest (`manifest.json` where the preferences hold none), and the unpacked directories under `extensions/`
//! that Chrome has not written into the preferences yet (their id is Chrome's id for the path, [`unpacked_id`]).
//!
//! Chrome's own component extensions (location 5 and 10: the PDF viewer and the like) are not listed.
use harness_proto::browser::Extension;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Chrome's `ManifestLocation` values that mean the Web Store or another packed install, and an unpacked load.
fn source(location: i64) -> Option<&'static str> {
    match location {
        1 | 2 | 3 | 6 | 7 | 9 => Some("store"),
        4 | 8 => Some("unpacked"),
        _ => None, // 0 invalid, 5 and 10 component: Chrome's own
    }
}

/// The unpacked extension directories under `dir/extensions/`.
///
/// # Errors
/// A directory that exists but cannot be read, naming it.
pub fn unpacked_dirs(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let root = dir.join("extensions");
    let rd = match std::fs::read_dir(&root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()), // alloc: none there
        Err(e) => return Err(format!("reading the unpacked extensions directory {}: {e}", root.display())),
    };
    let mut out = Vec::new(); // alloc: the directories, once at start or per listing
    for entry in rd {
        let entry = entry.map_err(|e| format!("reading {}: {e}", root.display()))?;
        let p = entry.path();
        if p.join("manifest.json").is_file() {
            let abs = std::fs::canonicalize(&p).map_err(|e| format!("resolving the unpacked extension {}: {e}", p.display()))?;
            out.push(abs);
        }
    }
    out.sort();
    Ok(out)
}

fn read_json(path: &Path) -> Result<Option<Map<String, Value>>, String> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).map(Some).map_err(|e| format!("{} is not a JSON object: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("reading {}: {e}", path.display())),
    }
}

/// `__MSG_name__` resolved through the extension's `_locales/<default_locale>/messages.json`, as Chrome shows it.
fn localized(text: &str, root: &Path, manifest: &Map<String, Value>) -> Result<String, String> {
    let Some(key) = text.strip_prefix("__MSG_").and_then(|t| t.strip_suffix("__")) else { return Ok(text.to_owned()) }; // alloc: the name, once per extension
    let Some(locale) = manifest.get("default_locale").and_then(Value::as_str) else {
        return Err(format!("the extension at {} names its {text} but has no default_locale", root.display()));
    };
    let path = root.join("_locales").join(locale).join("messages.json");
    let messages = read_json(&path)?.ok_or_else(|| format!("the extension at {} names {text} but {} is not there", root.display(), path.display()))?;
    let found = messages.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).and_then(|(_, v)| v.get("message")).and_then(Value::as_str);
    found.map(str::to_owned).ok_or_else(|| format!("{} has no message {key} (the extension at {} names {text})", path.display(), root.display()))
}

fn popup(manifest: &Map<String, Value>) -> Option<String> {
    ["action", "browser_action"].iter().find_map(|k| manifest.get(*k)?.get("default_popup")?.as_str()).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn from_manifest(id: String, root: &Path, manifest: &Map<String, Value>, enabled: bool, source: &str) -> Result<Extension, String> {
    let name = manifest.get("name").and_then(Value::as_str).ok_or_else(|| format!("the manifest of extension {id} at {} has no name", root.display()))?;
    let version = manifest.get("version").and_then(Value::as_str).ok_or_else(|| format!("the manifest of extension {id} at {} has no version", root.display()))?;
    Ok(Extension {
        name: localized(name, root, manifest)?,
        version: version.to_owned(), // alloc: the version
        enabled,
        popup: popup(manifest),
        source: source.to_owned(), // alloc: the source word
        path: root.display().to_string(),
        id,
    })
}

/// Whether a preferences entry is enabled: `disable_reasons` empty or 0, and `state` (older profiles) not 0.
fn enabled(entry: &Map<String, Value>) -> bool {
    let reasons_clear = match entry.get("disable_reasons") {
        None | Some(Value::Null) => true,
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Number(n)) => n.as_i64() == Some(0),
        Some(_) => false,
    };
    reasons_clear && entry.get("state").and_then(Value::as_i64) != Some(0)
}

/// Every extension in the profile at `dir/profile` and under `dir/extensions/`, by id.
///
/// # Errors
/// A preferences file or manifest that cannot be read or parsed, naming it.
pub fn list(dir: &Path) -> Result<Vec<Extension>, String> {
    let default = dir.join("profile").join("Default");
    let mut by_id: BTreeMap<String, Extension> = BTreeMap::new();
    for file in ["Preferences", "Secure Preferences"] {
        let path = default.join(file);
        let Some(prefs) = read_json(&path)? else { continue };
        let Some(settings) = prefs.get("extensions").and_then(|e| e.get("settings")).and_then(Value::as_object) else { continue };
        for (id, entry) in settings {
            let Some(entry) = entry.as_object() else { continue };
            let Some(src) = entry.get("location").and_then(Value::as_i64).and_then(source) else { continue };
            let Some(rel) = entry.get("path").and_then(Value::as_str) else { continue };
            let root = if Path::new(rel).is_absolute() { PathBuf::from(rel) } else { default.join("Extensions").join(rel) };
            let manifest = match entry.get("manifest").and_then(Value::as_object) {
                Some(m) => m.clone(),
                None => read_json(&root.join("manifest.json"))?
                    .ok_or_else(|| format!("extension {id} ({} in {}) has no manifest.json at {}", src, path.display(), root.display()))?,
            };
            let ext = from_manifest(id.clone(), &root, &manifest, enabled(entry), src)?;
            by_id.insert(id.clone(), ext);
        }
    }
    for root in unpacked_dirs(dir)? {
        let id = unpacked_id(&root);
        if by_id.contains_key(&id) {
            continue;
        }
        let mpath = root.join("manifest.json");
        let manifest = read_json(&mpath)?.ok_or_else(|| format!("{} went away while listing extensions", mpath.display()))?;
        by_id.insert(id.clone(), from_manifest(id, &root, &manifest, true, "unpacked")?);
    }
    Ok(by_id.into_values().collect())
}

/// Chrome's id for an unpacked extension at absolute path `path` (crx_file::id_util::GenerateIdForPath): the SHA-256
/// of the path's bytes, its first 16 bytes in hex, each hex digit 0-f written a-p.
#[must_use]
pub fn unpacked_id(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let h = sha256(path.as_os_str().as_bytes());
    h[..16].iter().flat_map(|b| [b >> 4, b & 0xf]).map(|n| char::from(b'a' + n)).collect()
}

/// SHA-256 (FIPS 180-4), for [`unpacked_id`] only.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec(); // alloc: one path's bytes, padded
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, c) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let chv = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(chv).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (i, x) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn sha256_vectors() {
        assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn ids_are_32_letters_a_to_p() {
        let id = unpacked_id(Path::new("/Users/a/.harness/browser/extensions/x"));
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|c| (b'a'..=b'p').contains(&c)));
        assert_ne!(id, unpacked_id(Path::new("/Users/a/.harness/browser/extensions/y")));
    }

    #[test]
    fn lists_preferences_and_unpacked() {
        let dir = std::env::temp_dir().join(format!("harness-browser-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = dir.join("profile/Default/Extensions/abc/1.0");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("manifest.json"), r#"{"name":"Store One","version":"1.0","action":{"default_popup":"p.html"}}"#).unwrap();
        std::fs::write(
            dir.join("profile/Default/Secure Preferences"),
            r#"{"extensions":{"settings":{"abc":{"location":1,"path":"abc/1.0","disable_reasons":[]},"pdf":{"location":5,"path":"/x"}}}}"#,
        )
        .unwrap();
        let un = dir.join("extensions/mine");
        std::fs::create_dir_all(un.join("_locales/en")).unwrap();
        std::fs::write(un.join("manifest.json"), r#"{"name":"__MSG_n__","version":"2","default_locale":"en"}"#).unwrap();
        std::fs::write(un.join("_locales/en/messages.json"), r#"{"N":{"message":"Mine"}}"#).unwrap();
        let l = list(&dir).unwrap();
        assert_eq!(l.len(), 2, "{l:?}");
        let s = l.iter().find(|e| e.id == "abc").unwrap();
        assert_eq!((s.name.as_str(), s.popup.as_deref(), s.source.as_str(), s.enabled), ("Store One", Some("p.html"), "store", true));
        let u = l.iter().find(|e| e.source == "unpacked").unwrap();
        assert_eq!(u.name, "Mine");
        assert_eq!(u.id, unpacked_id(&std::fs::canonicalize(&un).unwrap()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
