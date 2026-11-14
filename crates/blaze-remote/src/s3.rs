//! S3 requests: AWS Signature Version 4 (HMAC-SHA256 by `ring`, region `auto` as R2 takes it,
//! path-style `https://<endpoint>/<bucket>/<name>`), and the small XML of multipart uploads.
//! Pure functions: the time is given.
//!
//! Signed headers are `host`, `x-amz-content-sha256`, `x-amz-date`, and any `if-match` or
//! `if-none-match`. No test vector checks the signing here: AWS's published vectors use an
//! example secret, which is never written into this tree. Correctness against R2 is not
//! measured.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use ring::{digest, hmac};

use crate::settings::Credentials;

/// A request to send: its address and headers (the host one included).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Signed {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

fn hex(bytes: &[u8]) -> String {
    blaze_format::hex::encode(bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, bytes).as_ref())
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), message).as_ref().to_vec()
}

/// RFC 3986 encoding as SigV4 wants it: unreserved bytes kept, `/` kept when `path`.
pub fn encode(text: &str, path: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') || (path && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The civil date and time of `unix_s`, as `YYYYMMDDTHHMMSSZ` (Howard Hinnant's
/// days-to-civil).
pub fn amz_date(unix_s: u64) -> String {
    let days = (unix_s / 86_400) as i64;
    let secs = unix_s % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
}

/// The host of an endpoint `https://host[:port]`.
pub fn host_of(endpoint: &str) -> &str {
    endpoint.trim_start_matches("https://").trim_end_matches('/')
}

/// Signs `method` of `name` in `bucket` at `endpoint`, with `query` (unencoded pairs) and the
/// conditional headers `extra`, over `body`.
pub fn sign(method: &str, endpoint: &str, bucket: &str, name: &str, query: &[(&str, &str)], extra: &[(&str, String)], body: &[u8], credentials: &Credentials, unix_s: u64) -> Signed {
    let host = host_of(endpoint);
    let date_time = amz_date(unix_s);
    let date = &date_time[..8];
    let payload = sha256_hex(body);
    let uri = format!("/{}/{}", encode(bucket, false), encode(name, true));
    let mut pairs: Vec<(String, String)> = query.iter().map(|(k, v)| (encode(k, false), encode(v, false))).collect();
    pairs.sort();
    let canonical_query = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let mut headers: Vec<(String, String)> = vec![("host".to_owned(), host.to_owned()), ("x-amz-content-sha256".to_owned(), payload.clone()), ("x-amz-date".to_owned(), date_time.clone())];
    for (k, v) in extra {
        headers.push(((*k).to_ascii_lowercase(), v.trim().to_owned()));
    }
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical = format!("{method}\n{uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload}");
    let scope = format!("{date}/auto/s3/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{date_time}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
    let k_date = hmac_sha256(format!("AWS4{}", credentials.secret.expose()).as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, b"auto");
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex(&hmac_sha256(&k_signing, to_sign.as_bytes()));
    headers.push(("authorization".to_owned(), format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}", credentials.access_key_id)));
    let url = if canonical_query.is_empty() { format!("https://{host}{uri}") } else { format!("https://{host}{uri}?{canonical_query}") };
    Signed { url, headers }
}

/// An unsigned address of a public object: `<public_url>/<name>`.
pub fn public(public_url: &str, name: &str) -> String {
    format!("{}/{}", public_url.trim_end_matches('/'), encode(name, true))
}

/// The upload id in an `InitiateMultipartUploadResult`.
pub fn upload_id(xml: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(xml).ok()?;
    let start = text.find("<UploadId>")? + "<UploadId>".len();
    let end = start + text[start..].find("</UploadId>")?;
    let id = &text[start..end];
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_graphic() && b != b'<' && b != b'&')).then(|| id.to_owned())
}

/// The body of a `CompleteMultipartUpload`.
pub fn complete_body(parts: &[(u32, String)]) -> Vec<u8> {
    let mut out = String::from("<CompleteMultipartUpload>");
    for (n, etag) in parts {
        out.push_str(&format!("<Part><PartNumber>{n}</PartNumber><ETag>{}</ETag></Part>", etag.replace('&', "&amp;").replace('<', "&lt;")));
    }
    out.push_str("</CompleteMultipartUpload>");
    out.into_bytes()
}

/// Whether a 200 answer's body is an S3 error (CompleteMultipartUpload can answer one).
pub fn is_error(xml: &[u8]) -> bool {
    std::str::from_utf8(xml).is_ok_and(|text| text.contains("<Error>"))
}
