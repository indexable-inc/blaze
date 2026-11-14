//! The WebTransport probe: does a browser connect to a WebTransport server on 127.0.0.1 from an
//! https page, pinning a self-signed certificate by its hash? An echo server with the
//! explorer's own certificate (`ix_prof::explore::cert`): it prints the link to the probe page
//! (`probe/index.html` in this crate, served from an https origin), and echoes each
//! bidirectional stream on itself, each unidirectional stream on one it opens, and each
//! datagram. It prints what each session's CONNECT carried (path, Origin, User-Agent), so a run
//! also says what Origin each browser sends.
//!
//! The echo is [`blaze_explore::probe::Echo`], a sans-IO machine; this program is its thin
//! driver: blaze-http's HTTP/3 driver loop on one blaze-core native driver, one poll a step.
//!
//! Its input is one optional JSON value (inline, a file's path, or `-` for stdin):
//! `{"page": "<the probe page's URL>", "port": <0 for any>, "sessions": <n>}`. With `sessions`,
//! it stops once that many sessions ended, and exits nonzero naming each part (bidirectional,
//! unidirectional, datagrams) of each session that failed; without, Ctrl-C stops it.

#![feature(allocator_api)]

use std::{
    alloc::Global,
    io::Read as _,
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    process::ExitCode,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use blaze_core::{Driver as _, wait::Deadline};
use blaze_explore::probe::{Echo, Ended};
use blaze_http::h3::{Endpoint, ServerConfig, Transport, driver::Loop};
use ix_prof::explore::cert::{Cert, Validity};

/// Where the probe page is published: indexable-inc/ix serves `probe/index.html` there.
const PAGE: &str = "https://ix.dev/explore/probe";
/// How long the socket may take to bind.
const START_WAIT: Duration = Duration::from_secs(10);
/// The longest one poll waits with nothing due, so a wait never goes unbounded.
const POLL_MOST: Duration = Duration::from_secs(30);

#[derive(Debug, serde_derive::Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    #[serde(default = "page")]
    page: String,
    #[serde(default)]
    port: u16,
    /// Stop after this many sessions ended; none runs until Ctrl-C.
    #[serde(default)]
    sessions: Option<u32>,
}

fn page() -> String {
    PAGE.to_owned()
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("blaze-explore-probe: {why}");
            ExitCode::FAILURE
        }
    }
}

fn input() -> Result<Input, String> {
    let mut args = std::env::args().skip(1);
    let text = match (args.next(), args.next()) {
        (None, _) => "{}".to_owned(),
        (Some(_), Some(extra)) => {
            return Err(format!("it takes one JSON value, and `{extra}` is a second"));
        }
        (Some(arg), None) if arg == "-" => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text).map_err(|why| format!("stdin: {why}"))?;
            text
        }
        (Some(arg), None) if arg.trim_start().starts_with('{') => arg,
        (Some(path), None) => {
            std::fs::read_to_string(&path).map_err(|why| format!("{path}: {why}"))?
        }
    };
    serde_json::from_str(&text)
        .map_err(|why| format!("the input `{text}` is not {{\"page\", \"port\", \"sessions\"}}: {why}"))
}

fn run() -> Result<(), String> {
    let input = input()?;
    if !input.page.starts_with("https://") {
        return Err(format!(
            "the page `{}` is not https: a browser opens WebTransport only from a secure page",
            input.page
        ));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|why| format!("the wall clock: {why}"))?
        .as_secs();
    let cert = Cert::make(Validity::starting(now), blaze_explore::NAME)
        .map_err(|why| why.to_string())?;
    let identity = cert.identity().map_err(|why| why.to_string())?;
    let config = ServerConfig {
        identity,
        transport: Transport { idle: Duration::from_secs(30), keep_alive: Some(Duration::from_secs(5)) },
        requests: false,
    };
    // No file effect is asked: the root is never read.
    let mut driver = blaze_core::native(Path::new("/"), 1)
        .map_err(|why| format!("blaze-core's native driver: {why}"))?;
    // quinn-proto's clock is an `Instant`: the epoch is read once, right after the native
    // driver whose clock (`Driver::now`, from its start) it stands for.
    let epoch = Instant::now();
    let endpoint = Endpoint::new(epoch, Some(config), None).map_err(|why| why.to_string())?;
    let at = SocketAddr::from((Ipv4Addr::LOCALHOST, input.port));
    let mut core = Loop::new(endpoint, 0, at);
    let mut effects = Vec::new_in(Global);
    let mut done = Vec::new_in(Global);
    // Bounded by START_WAIT on the driver's clock.
    let local = loop {
        if let Some(local) = core.local() {
            break local;
        }
        if let Some(error) = core.failed() {
            return Err(format!("binding the UDP socket at {at}: {error}"));
        }
        if driver.now() > START_WAIT {
            return Err(format!("binding the UDP socket at {at}: not bound in {START_WAIT:?}"));
        }
        core.step(driver.now(), &mut effects);
        driver.submit(&mut effects);
        {
            let _wait = tracing::info_span!(target: "blaze::explore", "bind the probe's socket", wait = "driver").entered();
            driver.poll(&mut done, Deadline::after(driver.now(), START_WAIT));
        }
        for completion in done.drain(..) {
            core.complete(completion);
        }
    };
    println!("{}#127.0.0.1:{}/{}", input.page, local.port(), cert.hash);
    println!(
        "certificate valid until {} (Unix seconds); {}",
        cert.validity.not_after_s,
        match input.sessions {
            Some(n) => format!("the probe stops after {n} sessions"),
            None => "Ctrl-C stops the probe".to_owned(),
        }
    );
    let mut echo = Echo::new();
    let mut said = Vec::new_in(Global);
    let mut ended: Vec<Ended> = Vec::new_in(Global);
    // Each turn's wait is bounded by POLL_MOST; the loop ends when `sessions` ended, the socket
    // fails, or Ctrl-C ends the process.
    loop {
        let now = driver.now();
        core.endpoint().advance_clock(now);
        echo.step(core.endpoint());
        echo.take_said(&mut said);
        for line in said.drain(..) {
            println!("{line}");
        }
        echo.take_ended(&mut ended);
        if input.sessions.is_some_and(|n| ended.len() >= n as usize) {
            return verdict(&ended);
        }
        if let Some(error) = core.failed() {
            return Err(format!("the probe's socket at {local}: {error}"));
        }
        core.step(now, &mut effects);
        driver.submit(&mut effects);
        {
            let _wait = tracing::info_span!(target: "blaze::explore", "wait for the probe's socket and timers", wait = "driver").entered();
            driver.poll(&mut done, core.deadline().min(Deadline::after(now, POLL_MOST)));
        }
        // What came is received at the time it came, not at the time the poll began.
        core.endpoint().advance_clock(driver.now());
        for completion in done.drain(..) {
            core.complete(completion);
        }
    }
}

/// Every session's parts that failed, named; none is a pass.
fn verdict(ended: &[Ended]) -> Result<(), String> {
    let mut failed = Vec::new_in(Global);
    for session in ended {
        for (part, tally) in session.failed() {
            failed.push(format!(
                "session from {} ({}): {part}: {} failed, {} echoed",
                session.peer, session.closed, tally.failed, tally.echoed
            ));
        }
    }
    if failed.is_empty() {
        println!("{} sessions ended, every echo whole", ended.len());
        Ok(())
    } else {
        Err(failed.join("; "))
    }
}
