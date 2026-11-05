//! QPACK (RFC 9204) with the static table only. Both sides of a blaze connection advertise a
//! dynamic table of capacity 0, so a field section that references one is refused, never
//! guessed at. Strings are read with or without Huffman, and written without.

#![blaze::sans_io]

use super::huffman;

/// The most fields one section may hold.
pub(crate) const MOST_FIELDS: usize = 128;

/// RFC 9204's static table (Appendix A), index by index.
pub(crate) const STATIC: [(&str, &str); 99] = [
    (":authority", ""),
    (":path", "/"),
    ("age", "0"),
    ("content-disposition", ""),
    ("content-length", "0"),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("referer", ""),
    ("set-cookie", ""),
    (":method", "CONNECT"),
    (":method", "DELETE"),
    (":method", "GET"),
    (":method", "HEAD"),
    (":method", "OPTIONS"),
    (":method", "POST"),
    (":method", "PUT"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "103"),
    (":status", "200"),
    (":status", "304"),
    (":status", "404"),
    (":status", "503"),
    ("accept", "*/*"),
    ("accept", "application/dns-message"),
    ("accept-encoding", "gzip, deflate, br"),
    ("accept-ranges", "bytes"),
    ("access-control-allow-headers", "cache-control"),
    ("access-control-allow-headers", "content-type"),
    ("access-control-allow-origin", "*"),
    ("cache-control", "max-age=0"),
    ("cache-control", "max-age=2592000"),
    ("cache-control", "max-age=604800"),
    ("cache-control", "no-cache"),
    ("cache-control", "no-store"),
    ("cache-control", "public, max-age=31536000"),
    ("content-encoding", "br"),
    ("content-encoding", "gzip"),
    ("content-type", "application/dns-message"),
    ("content-type", "application/javascript"),
    ("content-type", "application/json"),
    ("content-type", "application/x-www-form-urlencoded"),
    ("content-type", "image/gif"),
    ("content-type", "image/jpeg"),
    ("content-type", "image/png"),
    ("content-type", "text/css"),
    ("content-type", "text/html; charset=utf-8"),
    ("content-type", "text/plain"),
    ("content-type", "text/plain;charset=utf-8"),
    ("range", "bytes=0-"),
    ("strict-transport-security", "max-age=31536000"),
    ("strict-transport-security", "max-age=31536000; includesubdomains"),
    ("strict-transport-security", "max-age=31536000; includesubdomains; preload"),
    ("vary", "accept-encoding"),
    ("vary", "origin"),
    ("x-content-type-options", "nosniff"),
    ("x-xss-protection", "1; mode=block"),
    (":status", "100"),
    (":status", "204"),
    (":status", "206"),
    (":status", "302"),
    (":status", "400"),
    (":status", "403"),
    (":status", "421"),
    (":status", "425"),
    (":status", "500"),
    ("accept-language", ""),
    ("access-control-allow-credentials", "FALSE"),
    ("access-control-allow-credentials", "TRUE"),
    ("access-control-allow-headers", "*"),
    ("access-control-allow-methods", "get"),
    ("access-control-allow-methods", "get, post, options"),
    ("access-control-allow-methods", "options"),
    ("access-control-expose-headers", "content-length"),
    ("access-control-request-headers", "content-type"),
    ("access-control-request-method", "get"),
    ("access-control-request-method", "post"),
    ("alt-svc", "clear"),
    ("authorization", ""),
    ("content-security-policy", "script-src 'none'; object-src 'none'; base-uri 'none'"),
    ("early-data", "1"),
    ("expect-ct", ""),
    ("forwarded", ""),
    ("if-range", ""),
    ("origin", ""),
    ("purpose", "prefetch"),
    ("server", ""),
    ("timing-allow-origin", "*"),
    ("upgrade-insecure-requests", "1"),
    ("user-agent", ""),
    ("x-forwarded-for", ""),
    ("x-frame-options", "deny"),
    ("x-frame-options", "sameorigin"),
];

/// Appends the field section of `fields`, names lowercase: each field as the static table's
/// entry when it has the pair, as a literal with the table's name when it has the name, and as
/// a literal otherwise. Required Insert Count and Base are 0.
pub(crate) fn encode(fields: &[(&str, &str)], out: &mut Vec<u8>) {
    out.extend_from_slice(&[0, 0]);
    for &(name, value) in fields {
        let pair = STATIC.iter().position(|&(n, v)| n == name && v == value);
        let named = STATIC.iter().position(|&(n, _)| n == name);
        match (pair, named) {
            // Indexed field line, static: 1 1 index(6).
            (Some(at), _) => put_int(out, 6, 0b1100_0000, at as u64),
            // Literal with name reference, static, may be indexed: 0 1 0 1 index(4).
            (None, Some(at)) => {
                put_int(out, 4, 0b0101_0000, at as u64);
                put_string(out, value.as_bytes());
            }
            // Literal with literal name, no Huffman: 0 0 1 0 0 length(3).
            (None, None) => {
                put_int(out, 3, 0b0010_0000, name.len() as u64);
                out.extend_from_slice(name.as_bytes());
                put_string(out, value.as_bytes());
            }
        }
    }
}

/// The fields of the section `block`, in order, or why it is not one this endpoint reads.
pub(crate) fn decode(block: &[u8]) -> Result<Vec<(String, String)>, String> {
    let mut at = 0;
    let (_, required) = get_int(block, &mut at, 8)?;
    if required != 0 {
        // alloc: an error's message, made only when it fails.
        return Err(dynamic("its Required Insert Count is not 0"));
    }
    // The Base: its sign and delta mean nothing with no table.
    get_int(block, &mut at, 7)?;
    // alloc: one section's fields, a CONNECT's or its answer's.
    let mut fields = Vec::new();
    while at < block.len() {
        if fields.len() == MOST_FIELDS {
            // alloc: an error's message, made only when it fails.
            return Err(format!("the field section has more than {MOST_FIELDS} fields"));
        }
        let first = block[at];
        let field = if first & 0b1000_0000 != 0 {
            if first & 0b0100_0000 == 0 {
                // alloc: an error's message, made only when it fails.
                return Err(dynamic("an indexed field line names the dynamic table"));
            }
            let (_, index) = get_int(block, &mut at, 6)?;
            let (name, value) = entry(index)?;
            // alloc: one field of a section.
            (name.to_owned(), value.to_owned())
        } else if first & 0b0100_0000 != 0 {
            if first & 0b0001_0000 == 0 {
                // alloc: an error's message, made only when it fails.
                return Err(dynamic("a literal's name reference names the dynamic table"));
            }
            let (_, index) = get_int(block, &mut at, 4)?;
            let (name, _) = entry(index)?;
            let value = get_string(block, &mut at, 7)?;
            // alloc: one field of a section.
            (name.to_owned(), value)
        } else if first & 0b0010_0000 != 0 {
            let name = get_string(block, &mut at, 3)?;
            if name.bytes().any(|it| it.is_ascii_uppercase()) {
                // alloc: an error's message, made only when it fails.
                return Err(format!(
                    "the field name `{name}` has uppercase letters (RFC 9114, 4.2)"
                ));
            }
            let value = get_string(block, &mut at, 7)?;
            (name, value)
        } else {
            // alloc: an error's message, made only when it fails.
            return Err(dynamic("a field line uses a post-base index"));
        };
        fields.push(field);
    }
    Ok(fields)
}

fn dynamic(what: &str) -> String {
    // alloc: an error's message, made only when it fails.
    format!(
        "{what}: the field section references QPACK's dynamic table, which this endpoint advertised as empty"
    )
}

fn entry(index: u64) -> Result<(&'static str, &'static str), String> {
    // alloc: an error's message, made only when it fails.
    usize::try_from(index)
        .ok()
        .and_then(|it| STATIC.get(it).copied())
        .ok_or_else(|| format!("static table index {index} is past its 99 entries"))
}

/// Appends `value` as an integer with an `n`-bit prefix (RFC 7541, 5.1), `flags` in the
/// first byte's high bits.
fn put_int(out: &mut Vec<u8>, n: u32, flags: u8, value: u64) {
    let most = (1u64 << n) - 1;
    if value < most {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | most as u8);
    let mut rest = value - most;
    while rest >= 0x80 {
        out.push((rest as u8 & 0x7f) | 0x80);
        rest >>= 7;
    }
    out.push(rest as u8);
}

/// A string with a 7-bit length prefix and no Huffman.
fn put_string(out: &mut Vec<u8>, bytes: &[u8]) {
    put_int(out, 7, 0, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// The integer with an `n`-bit prefix at `*at`, and its first byte's bits above the prefix.
fn get_int(block: &[u8], at: &mut usize, n: u32) -> Result<(u8, u64), String> {
    // alloc: an error's message, made only when it fails.
    let cut = || "the field section ends inside an integer".to_owned();
    let first = *block.get(*at).ok_or_else(cut)?;
    *at += 1;
    let most = (1u64 << n) - 1;
    let flags = if n == 8 { 0 } else { first >> n };
    let mut value = u64::from(first) & most;
    if value < most {
        return Ok((flags, value));
    }
    let mut shift = 0u32;
    loop {
        let byte = *block.get(*at).ok_or_else(cut)?;
        *at += 1;
        if shift > 56 {
            // alloc: an error's message, made only when it fails.
            return Err("an integer in the field section is past 64 bits".to_owned());
        }
        value = value
            .checked_add(u64::from(byte & 0x7f) << shift)
            // alloc: an error's message, made only when it fails.
            .ok_or_else(|| "an integer in the field section is past 64 bits".to_owned())?;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok((flags, value));
        }
    }
}

/// The string with an `n`-bit length prefix at `*at`, its Huffman bit just above the prefix.
fn get_string(block: &[u8], at: &mut usize, n: u32) -> Result<String, String> {
    let (flags, len) = get_int(block, at, n)?;
    let end =
        usize::try_from(len).ok().and_then(|it| at.checked_add(it)).filter(|&it| it <= block.len());
    // alloc: an error's message, made only when it fails.
    let end = end.ok_or_else(|| format!("a string of {len} bytes runs past the field section"))?;
    let raw = &block[*at..end];
    *at = end;
    let bytes = if flags & 1 == 1 {
        // alloc: one decoded string, at most 8/5 of the section.
        let mut out = Vec::with_capacity(raw.len() * 8 / 5 + 1);
        huffman::decode(raw, &mut out).map_err(str::to_owned)?;
        out
    } else {
        // alloc: one string of a section.
        raw.to_vec()
    };
    // alloc: an error's message, made only when it fails.
    String::from_utf8(bytes).map_err(|_| "a field is not UTF-8".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connect_and_its_answer_round_trip() {
        let fields = [
            (":method", "CONNECT"),
            (":protocol", "webtransport"),
            (":scheme", "https"),
            (":authority", "127.0.0.1:4433"),
            (":path", "/explore/v1"),
            ("origin", "https://ix.dev"),
            ("sec-webtransport-http3-draft02", "1"),
        ];
        let mut block = Vec::new();
        encode(&fields, &mut block);
        let back = decode(&block).unwrap();
        assert_eq!(back.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect::<Vec<_>>(), fields);
        let mut ok = Vec::new();
        encode(&[(":status", "200")], &mut ok);
        assert_eq!(ok, [0, 0, 0xd9], "`:status 200` is static entry 25");
        let mut forbidden = Vec::new();
        encode(&[(":status", "403")], &mut forbidden);
        assert_eq!(decode(&forbidden).unwrap(), [(":status".to_owned(), "403".to_owned())]);
    }

    /// A field as Chrome writes it: a literal with a static name reference and a Huffman value
    /// (`www.example.com`, RFC 7541's C.4.1), and a long index with a continuation byte.
    #[test]
    fn huffman_values_and_long_indexes_are_read() {
        let mut block = vec![0, 0, 0x50, 0x80 | 12];
        block.extend_from_slice(&[
            0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff,
        ]);
        // Indexed, static, index 95 (`user-agent`): 0xff then 95 - 63 = 32.
        block.extend_from_slice(&[0xff, 32]);
        let fields = decode(&block).unwrap();
        assert_eq!(fields[0], (":authority".to_owned(), "www.example.com".to_owned()));
        assert_eq!(fields[1], ("user-agent".to_owned(), String::new()));
    }

    #[test]
    fn the_dynamic_table_is_refused_by_name() {
        for block in [&[1u8, 0][..], &[0, 0, 0x80], &[0, 0, 0x40, 0], &[0, 0, 0x10], &[0, 0, 0x00]]
        {
            let why = decode(block).unwrap_err();
            assert!(why.contains("dynamic table"), "{block:?}: {why}");
        }
        assert!(decode(&[0, 0, 0xc0 | 0x3f, 0x7f]).unwrap_err().contains("past its 99 entries"));
        assert!(decode(&[0, 0, 0x27, 0x01]).unwrap_err().contains("runs past"));
    }
}
