//! Times GETs through blaze's one HTTP client ([`HyperHttp`]) over the protocol a run picks, for
//! harness/netbench.py, which runs it and reports. One JSON value in, one JSON line per request
//! out, then one per pass:
//!
//!     blaze run -p blaze-registry --release --example netbench -- '<json>'
//!
//!     {"http3": "off" | "auto" | "require", "urls": ["https://..."], "concurrency": 16,
//!      "passes": 2, "timeout_ms": 30000}
//!
//! Each pass asks every URL once, `concurrency` at a time, on the same client: the first pass
//! makes its connections (cold: DNS, TCP or QUIC, TLS), the later ones reuse them (warm). No
//! adverts are kept on disk, so a run starts as a fresh machine would; `"auto"` learns adverts
//! within the run (DNS HTTPS records, `Alt-Svc`).
//!
//!     {"pass": 0, "url": "...", "ms": 12.3, "status": 200, "bytes": 4567, "error": null}
//!     {"pass": 0, "wall_ms": 345.6, "requests": 40, "failed": 0}

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Context as _;
use blaze_registry::http::{Http, Http3, HyperHttp, Net};

#[derive(serde_derive::Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    http3: String,
    urls: Vec<String>,
    concurrency: usize,
    passes: usize,
    timeout_ms: u64,
}

fn main() -> anyhow::Result<()> {
    let input = std::env::args().nth(1).context("usage: netbench '<json>'")?;
    let run: Run = serde_json::from_str(&input).context("the input")?;
    let http3 = Http3::parse(&run.http3).with_context(|| format!("`http3` is {}, not {:?}", Http3::NAMES, run.http3))?;
    anyhow::ensure!(run.concurrency > 0 && run.passes > 0, "`concurrency` and `passes` are at least 1");
    let mut net = Net::off();
    net.http3 = http3;
    let http = Arc::new(HyperHttp::with_net(Duration::from_millis(run.timeout_ms), net)?);
    let urls = Arc::new(run.urls);
    for pass in 0..run.passes {
        let next = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));
        let started = Instant::now();
        let workers: Vec<_> = (0..run.concurrency.min(urls.len()))
            .map(|i| {
                let (http, urls, next, failed) = (http.clone(), urls.clone(), next.clone(), failed.clone());
                std::thread::Builder::new()
                    .name(format!("netbench {i}"))
                    .spawn(move || {
                        loop {
                            let at = next.fetch_add(1, Ordering::Relaxed);
                            let Some(url) = urls.get(at) else { break };
                            let asked = Instant::now();
                            let got = http.get(url, &[]);
                            let ms = asked.elapsed().as_secs_f64() * 1000.0;
                            let line = match got {
                                Ok(answer) => serde_json::json!({
                                    "pass": pass, "url": url, "ms": ms, "status": answer.status,
                                    "bytes": answer.body.len(), "error": null,
                                }),
                                Err(e) => {
                                    failed.fetch_add(1, Ordering::Relaxed);
                                    serde_json::json!({
                                        "pass": pass, "url": url, "ms": ms, "status": null,
                                        "bytes": 0, "error": format!("{e:#}"),
                                    })
                                }
                            };
                            println!("{line}");
                        }
                    })
                    .context("starting a worker")
            })
            .collect::<anyhow::Result<_>>()?;
        for worker in workers {
            worker.join().map_err(|_| anyhow::anyhow!("a worker panicked"))?;
        }
        let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
        println!(
            "{}",
            serde_json::json!({"pass": pass, "wall_ms": wall_ms, "requests": urls.len(), "failed": failed.load(Ordering::Relaxed)})
        );
    }
    Ok(())
}
