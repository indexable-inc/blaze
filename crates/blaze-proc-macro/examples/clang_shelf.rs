//! D69: fetches the pinned C toolchain archive for this host to blaze's shelf, once.
//!
//! `clang_shelf <shelf>`: `<shelf>` is `<home>/.blaze/v2/toolchain/clang` (made by the caller).
//! The archive named by `pins/clang.json`'s row for this host is asked over
//! HTTPS with blaze-http (redirects followed, HTTPS only, the body held to the pinned size and
//! streamed, never whole in memory) into `incoming/<sha256>.tar.xz.part` through the effect
//! layer, its SHA-256 computed as it comes. The size and the digest are checked BEFORE the file
//! takes its name `<sha256>.tar.xz`: a mismatch removes it and stops naming both digests.
//! Nothing is unpacked or run here.

use std::{process::ExitCode, time::Duration};

use blaze_build_script::toolchain::clang_pin;
use blaze_pins::{CLANG_JSON, CLANG_PATH};
use blaze_http::{Event, Method, Request, driver::Handle};
use blaze_proc_macro::record::native_world;
use bstd::{fs::Durable, mem::TASK_FRAME, path::Path as BPath};

const HOST: &str = "aarch64-apple-darwin";
/// Bytes held before each append: one effect per piece of this size.
const PIECE: usize = 8 << 20;
/// Each append's budget.
const APPEND_BUDGET: Duration = Duration::from_secs(60);
/// How long one wait for the download's next event may last: past it, the download is stuck.
const WITHIN: Duration = Duration::from_secs(60);

fn hex(bytes: &[u8]) -> std::string::String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() -> ExitCode {
    let args: std::vec::Vec<std::string::String> = std::env::args().collect();
    let [_, shelf] = args.as_slice() else {
        eprintln!("usage: clang_shelf <home>/.blaze/v2/toolchain/clang");
        return ExitCode::from(2);
    };
    let pin = clang_pin(CLANG_JSON, HOST).unwrap_or_else(|error| panic!("{error}"));
    let want = hex(&pin.sha256);
    let url = format!("{}/{}", pin.release, pin.name);
    let (io, root) = native_world(std::path::Path::new(shelf)).unwrap_or_else(|error| panic!("the shelf {shelf}: {error}"));
    let cx = || io.cx(APPEND_BUDGET, TASK_FRAME).unwrap_or_else(|_| panic!("the shelf {shelf}: the world stopped"));
    let part_name = format!("incoming/{want}.tar.xz.part");
    let part = BPath::new(&part_name).expect("a relative path");
    let done_name = format!("{want}.tar.xz");
    let done = BPath::new(&done_name).expect("a relative path");
    let file = root.create(&cx(), &part).unwrap_or_else(|error| panic!("{shelf}/{part_name}: {error}"));

    let tls = blaze_http::roots::system(blaze_http::roots::Places { cert_file: None, cert_dir: None })
        .unwrap_or_else(|error| panic!("the machine's TLS roots: {error}"));
    let client = blaze_http::Client::new(blaze_http::Config::new(tls, "blaze-clang-shelf", Duration::from_secs(60)));
    let handle = Handle::spawn(client, 1).unwrap_or_else(|error| panic!("blaze-http's driver: {error}"));
    let mut request = Request::get(url.clone());
    request.method = Method::Get;
    request.gzip = false;
    request.most = pin.size;
    request.https_only = true;
    let mut answer = handle.ask(request);

    let started = std::time::Instant::now();
    let mut sha = blaze_sha256::Sha256::new();
    let mut held: std::vec::Vec<u8> = std::vec::Vec::with_capacity(PIECE);
    let mut got: u64 = 0;
    let mut told: u64 = 0;
    loop {
        let event = answer.next(WITHIN).unwrap_or_else(|unanswered| panic!("{url}: {unanswered}"));
        match event {
            Some(Event::Head { url: at, status, .. }) => {
                println!("answered by {at}: {status}");
                assert_eq!(status, 200, "{url}: answered {status} by {at}");
            }
            Some(Event::Body { bytes, .. }) => {
                sha.update(&bytes);
                got += bytes.len() as u64;
                held.extend_from_slice(&bytes);
                if held.len() >= PIECE {
                    file.append(&cx(), &held, Durable::No).unwrap_or_else(|error| panic!("{shelf}/{part_name}: {error}"));
                    held.clear();
                }
                if got >= told + (128 << 20) {
                    told = got;
                    println!("{got} of {} bytes, {:.0?}", pin.size, started.elapsed());
                }
            }
            Some(Event::Done { .. }) => break,
            Some(Event::Failed { error, .. }) => panic!("{url}: {error}"),
            None => panic!("{url}: blaze-http's driver stopped"),
        }
    }
    file.append(&cx(), &held, Durable::No).unwrap_or_else(|error| panic!("{shelf}/{part_name}: {error}"));
    file.close(&cx(), Durable::Yes).unwrap_or_else(|error| panic!("{shelf}/{part_name}: {error}"));
    let digest = hex(&sha.finish());
    if got != pin.size || digest != want {
        root.remove(&cx(), &part, Durable::Yes).unwrap_or_else(|error| panic!("{shelf}/{part_name}: {error}"));
        eprintln!(
            "{url}: REFUSED (D69): {got} bytes with sha256 {digest}, but {CLANG_PATH} pins {} bytes with sha256 {want}; the file is removed, nothing was unpacked",
            pin.size
        );
        return ExitCode::FAILURE;
    }
    root.rename_replacing_synced(&cx(), &part, &done).unwrap_or_else(|error| panic!("{shelf}/{done_name}: {error}"));
    println!("digest ok: {got} bytes, sha256 {digest} = {CLANG_PATH}'s {HOST} pin; kept as {shelf}/{done_name} in {:.1?}", started.elapsed());
    ExitCode::SUCCESS
}
