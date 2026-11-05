//! Many source files as one, for a page to fetch: for each file the length of its path, the
//! path, the length of its text and the text, lengths as little-endian `u32`s.

pub fn pack(files: &[(String, String)]) -> Vec<u8> {
    let mut bundle = Vec::new();
    for (path, text) in files {
        for part in [path, text] {
            bundle.extend((part.len() as u32).to_le_bytes());
            bundle.extend(part.as_bytes());
        }
    }
    bundle
}

pub fn unpack(mut bundle: &[u8]) -> Result<Vec<(String, String)>, String> {
    let mut files = Vec::new();
    while !bundle.is_empty() {
        let mut part = || {
            let (len, rest) = bundle.split_first_chunk::<4>().ok_or("truncated bundle")?;
            let len = u32::from_le_bytes(*len) as usize;
            let (part, rest) = rest.split_at_checked(len).ok_or("truncated bundle")?;
            bundle = rest;
            String::from_utf8(part.to_vec()).map_err(|e| e.to_string())
        };
        files.push((part()?, part()?));
    }
    Ok(files)
}
