mod client;
mod daemon;
mod paths;
mod profiles;
mod proto;
mod queries;
mod runs;
mod web;

use anyhow::{Result, bail};
use serde_json::json;

const USAGE: &str = "\
harness                       start the daemon if needed, open the web UI, return
harness claude|codex|shell [args...]
                              start a locked-down agent in this directory, open it in the web UI
harness tui [claude|codex|shell [args...]]
                              terminal UI instead of the browser (ctrl-] returns to the tree)
harness ls                    print the session tree
harness send <id> <text...>   type a message into a session and press Enter
harness screen <id>           print a session's current screen
harness kill <id>             kill a session's process
harness run '<cmd>'           run a shell command through the daemon (every pipeline stage's output is
                              stored), stream its output, exit with its exit code
harness runs [N]              list the N (default 20) most recent runs of every agent
harness out <run> [stage] [--err]
                              print a run's stored output: the caller-visible stdout, or one stage's
harness web                   print the web UI address
harness daemon [--port N]     run the daemon in the foreground (normally started automatically)
harness stop                  stop the daemon and every session";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).cloned().unwrap_or_default();
    match args.first().map(String::as_str) {
        None => open_web(None).await,
        Some(kind @ ("claude" | "codex" | "shell")) => {
            let (cols, rows) = crossterm::terminal::size().unwrap_or((140, 45));
            let cwd = std::env::current_dir()?.display().to_string();
            let v = client::Ctl::connect()
                .await?
                .req(json!({"op": "spawn", "kind": kind, "args": &args[1..], "cwd": cwd, "cols": cols, "rows": rows}))
                .await?;
            open_web(v["id"].as_u64()).await
        }
        Some("tui") => match args.get(1).map(String::as_str) {
            Some(kind @ ("claude" | "codex" | "shell")) => client::run(Some((kind.to_string(), args[2..].to_vec()))).await,
            _ => client::run(None).await,
        },
        Some("daemon") => {
            let port = match args.iter().position(|a| a == "--port") {
                Some(i) => arg(i + 1).parse()?,
                None => std::env::var("HARNESS_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(4747),
            };
            daemon::run(port).await
        }
        Some("ls") => {
            let v = client::Ctl::connect().await?.req(json!({"op": "list"})).await?;
            for (_, line, _) in client::tree_lines(v["sessions"].as_array().map(Vec::as_slice).unwrap_or(&[])) {
                println!("{line}");
            }
            Ok(())
        }
        Some("send") => {
            let id: u64 = arg(1).parse()?;
            client::Ctl::connect().await?.req(json!({"op": "send", "id": id, "text": args[2..].join(" ")})).await?;
            Ok(())
        }
        Some("screen") => {
            let v = client::Ctl::connect().await?.req(json!({"op": "screen", "id": arg(1).parse::<u64>()?})).await?;
            print!("{}", v["text"].as_str().unwrap_or(""));
            Ok(())
        }
        Some("kill") => {
            client::Ctl::connect().await?.req(json!({"op": "kill", "id": arg(1).parse::<u64>()?})).await?;
            Ok(())
        }
        Some("web") => {
            println!("{}", client::Ctl::connect().await?.req(json!({"op": "info"})).await?["web"].as_str().unwrap_or(""));
            Ok(())
        }
        Some("stop") => {
            let mut ctl = client::Ctl::connect().await?;
            let _ = ctl.req(json!({"op": "shutdown"})).await;
            Ok(())
        }
        Some("tee") => runs::tee(&arg(1)),
        Some("run") => {
            if args.len() < 2 {
                bail!("usage: harness run '<cmd>'");
            }
            std::process::exit(run_cmd(args[1..].join(" ")).await?)
        }
        Some("runs") => {
            let limit: usize = args.get(1).map(|a| a.parse()).transpose()?.unwrap_or(20);
            let v = client::Ctl::connect().await?.req(json!({"op": "runs", "limit": limit})).await?;
            for r in v["runs"].as_array().into_iter().flatten().rev() {
                let exit = r["exit_code"].as_i64().map(|c| c.to_string()).or_else(|| r["signal"].as_str().map(String::from)).unwrap_or("-".into());
                let session = r["session"].as_u64().map(|s| s.to_string()).unwrap_or("-".into());
                let n = r["stages"].as_array().map_or(0, Vec::len);
                let cmd: String = r["cmd"].as_str().unwrap_or("").replace('\n', " ").chars().take(90).collect();
                println!("{:>6}  s{:<4} {:<8} {:>4}  {}st  {}", r["id"].as_str().unwrap_or(""), session, r["status"].as_str().unwrap_or(""), exit, n, cmd);
            }
            Ok(())
        }
        Some("out") => {
            let stage = args.get(2).filter(|a| !a.starts_with("--")).map(|a| a.parse::<usize>()).transpose()?;
            let stream = if args.iter().any(|a| a == "--err") { "err" } else { "out" };
            let mut ctl = client::Ctl::connect().await?;
            let mut offset = 0u64;
            loop {
                let v = ctl.req(json!({"op": "run_read", "id": arg(1), "stage": stage, "stream": stream, "offset": offset})).await?;
                print!("{}", v["data"].as_str().unwrap_or(""));
                offset = v["next_offset"].as_u64().unwrap_or(offset);
                if v["eof"].as_bool() != Some(false) {
                    break;
                }
            }
            Ok(())
        }
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

/// Print the web UI address (deep-linked to a session if given) and open it in the browser.
async fn open_web(session: Option<u64>) -> Result<()> {
    let base = client::Ctl::connect().await?.req(json!({"op": "info"})).await?["web"].as_str().unwrap_or("").to_string();
    let url = match session {
        Some(id) => format!("{base}/#{id}"),
        None => base,
    };
    println!("{url}");
    if std::env::var_os("HARNESS_NO_OPEN").is_none() {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let _ = std::process::Command::new(opener).arg(&url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    }
    Ok(())
}

/// `harness run`: start the command in the daemon, stream its caller-visible stdout/stderr, return its exit code.
async fn run_cmd(cmd: String) -> Result<i32> {
    use std::io::{IsTerminal, Write};
    let cwd = std::env::current_dir()?.display().to_string();
    let env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let session = std::env::var("HARNESS_SESSION").ok().and_then(|s| s.parse::<u64>().ok());
    let v = client::Ctl::connect()
        .await?
        .req(json!({"op": "run", "cmd": cmd, "cwd": cwd, "env": env, "clear_env": true, "session": session, "parent": std::env::var("HARNESS_RUN").ok()}))
        .await?;
    let id = v["id"].as_str().unwrap_or_default().to_string();
    let pump = |stream: &'static str| {
        let id = id.clone();
        async move {
            let mut ctl = client::Ctl::connect().await?;
            let mut offset = 0u64;
            loop {
                let v = ctl.req(json!({"op": "run_read", "id": id, "stream": stream, "offset": offset, "wait_ms": 1000})).await?;
                let data = v["data"].as_str().unwrap_or("");
                if !data.is_empty() {
                    if stream == "out" {
                        let mut o = std::io::stdout().lock();
                        o.write_all(data.as_bytes())?;
                        o.flush()?;
                    } else {
                        let mut e = std::io::stderr().lock();
                        e.write_all(data.as_bytes())?;
                    }
                }
                offset = v["next_offset"].as_u64().unwrap_or(offset);
                if v["done"].as_bool() == Some(true) && v["eof"].as_bool() == Some(true) {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        }
    };
    tokio::try_join!(pump("out"), pump("err"))?;
    let m = client::Ctl::connect().await?.req(json!({"op": "run_status", "id": id})).await?;
    let code = match (m["exit_code"].as_i64(), m["signal"].as_str()) {
        (Some(c), _) => c as i32,
        (None, Some(_)) => 128 + 15,
        _ => 1,
    };
    if std::io::stderr().is_terminal() {
        eprintln!("\x1b[2m[{id} exit {code}; stages: harness out {id} <n>]\x1b[0m");
    }
    Ok(code)
}
