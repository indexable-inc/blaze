//! Web view: the session tree plus a live, typeable terminal per session (xterm.js over a websocket).
use crate::daemon::{Daemon, Ev};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::broadcast;

pub async fn serve(d: Arc<Daemon>, listener: tokio::net::TcpListener) {
    let app = Router::new()
        .route("/", get(|| async { Html(include_str!("web/index.html")) }))
        .route("/api/sessions", get(sessions))
        .route("/api/spawn", post(spawn))
        .route("/api/sessions/{id}/kill", post(kill))
        .route("/ws/{id}", get(ws))
        .with_state(d);
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("web server stopped: {e}");
    }
}

async fn sessions(State(d): State<Arc<Daemon>>) -> impl IntoResponse {
    Json(json!({ "sessions": d.list() }))
}

#[derive(Deserialize)]
struct SpawnBody {
    kind: String,
    parent: Option<u64>,
    prompt: Option<String>,
}

async fn spawn(State(d): State<Arc<Daemon>>, Json(b): Json<SpawnBody>) -> impl IntoResponse {
    let res = tokio::task::spawn_blocking(move || d.spawn(&b.kind, &[], b.parent, None, b.prompt.as_deref(), 120, 40)).await;
    match res {
        Ok(Ok(id)) => Json(json!({ "id": id })),
        Ok(Err(e)) => Json(json!({ "error": format!("{e:#}") })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

async fn kill(State(d): State<Arc<Daemon>>, Path(id): Path<u64>) -> impl IntoResponse {
    match d.get(id) {
        Ok(s) => {
            s.kill();
            Json(json!({ "ok": true }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

async fn ws(State(d): State<Arc<Daemon>>, Path(id): Path<u64>, up: WebSocketUpgrade) -> impl IntoResponse {
    up.on_upgrade(move |socket| async move {
        if let Err(e) = pump(d, id, socket).await {
            eprintln!("ws {id}: {e:#}");
        }
    })
}

/// Text frames carry JSON control ({cols, rows} first, {exit} at the end); binary frames carry terminal bytes.
async fn pump(d: Arc<Daemon>, id: u64, mut socket: WebSocket) -> anyhow::Result<()> {
    let s = d.get(id)?;
    let (cols, rows) = s.size();
    socket.send(Message::Text(json!({ "cols": cols, "rows": rows }).to_string().into())).await?;
    let (snap, mut rx) = s.snapshot_and_subscribe();
    socket.send(Message::Binary(snap.into())).await?;
    let exited = *s.exit.lock().unwrap();
    if let Some(code) = exited {
        socket.send(Message::Text(json!({ "exit": code }).to_string().into())).await?;
    }
    s.attached.fetch_add(1, Ordering::Relaxed);
    let res: anyhow::Result<()> = async {
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(Ev::Data(b)) => socket.send(Message::Binary(b)).await?,
                    Ok(Ev::Exit(code)) => socket.send(Message::Text(json!({ "exit": code }).to_string().into())).await?,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let (snap, new_rx) = s.snapshot_and_subscribe();
                        rx = new_rx;
                        socket.send(Message::Binary(snap.into())).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                msg = socket.recv() => match msg {
                    Some(Ok(Message::Binary(b))) => { let s = s.clone(); tokio::task::spawn_blocking(move || s.write(&b)).await??; }
                    Some(Ok(Message::Text(t))) => {
                        let v: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
                        if let (Some(c), Some(r)) = (v["resize"]["cols"].as_u64(), v["resize"]["rows"].as_u64()) {
                            s.resize(c as u16, r as u16)?;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                },
            }
        }
    }
    .await;
    s.attached.fetch_sub(1, Ordering::Relaxed);
    res
}
