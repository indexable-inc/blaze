use std::alloc::Global;

use super::{Roots, pem};

/// `der` as a PEM certificate block, its base64 cut at 64 columns.
fn block(der: &[u8]) -> String {
    let text = blaze_base64::STANDARD.encode_in(der, Global);
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in text.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap());
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

/// A bundle's certificates come back byte for byte, in order, once each; other blocks and
/// text between blocks are passed over.
#[test]
fn a_bundle_gives_each_certificate_once_and_passes_over_the_rest() {
    let a: Vec<u8> = (0..700u32).map(|i| (i * 7 + 3) as u8).collect();
    let b: Vec<u8> = (0..1301u32).map(|i| (i * 13) as u8).collect();
    let text = format!(
        "# a comment\n{}-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\nlabel\n{}{}",
        block(&a),
        block(&b),
        block(&a)
    );
    let mut roots = Roots::new_in(Global);
    assert_eq!(pem::certificates(text.as_bytes(), &mut roots, Global), Ok(3));
    let got: Vec<&[u8]> = roots.each().collect();
    assert_eq!(got, [&a[..], &b[..]]);
}

/// A broken certificate block is an error at its offset, never skipped.
#[test]
fn a_broken_block_is_refused_where_it_starts() {
    let good = block(&[1, 2, 3, 4, 5]);
    let mut roots = Roots::new_in(Global);
    let unterminated = format!("{good}-----BEGIN CERTIFICATE-----\nAQID\n");
    let err = pem::certificates(unterminated.as_bytes(), &mut roots, Global).unwrap_err();
    assert_eq!((err.at, err.why), (good.len(), pem::PemWhy::Unterminated));
    let bad = format!("{good}-----BEGIN CERTIFICATE-----\nA*ID\n-----END CERTIFICATE-----\n");
    let err = pem::certificates(bad.as_bytes(), &mut roots, Global).unwrap_err();
    assert_eq!(err.at, good.len());
    assert!(matches!(err.why, pem::PemWhy::Base64(_)), "{err:?}");
}

/// This Mac's store gives roots, and it is read only when asked: the roots one call makes
/// count one read of the store, and an empty set none.
#[cfg(target_os = "macos")]
#[test]
fn this_macs_store_gives_roots_and_is_read_only_when_asked() {
    let roots = super::system(super::Places::default(), Global).unwrap_or_else(|e| panic!("{e}"));
    assert!(roots.len() > 10, "{} roots", roots.len());
    // The count is the value's, not the process's: another test reading the store at the
    // same time does not move it.
    assert_eq!(roots.reads(), 1);
    assert_eq!(super::Roots::new_in(Global).reads(), 0, "nothing read makes an empty set");
    // Each root is a DER SEQUENCE.
    assert!(roots.each().all(|der| der.first() == Some(&0x30)));
}
