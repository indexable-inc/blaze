mod client;
mod daemon;
mod paths;
mod profiles;
mod proto;
mod queries;
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
