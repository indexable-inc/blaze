//! Wire format between `harness` clients (TUI, kernel) and `harnessd`: frames of
//! `[type: u8][len: u32 BE][payload]` over a Unix socket.
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// JSON request (client → daemon) or response (daemon → client).
pub const T_JSON: u8 = 1;
/// Terminal bytes: PTY output (daemon → client) or keyboard input (client → daemon).
pub const T_DATA: u8 = 2;
/// `{"cols":..,"rows":..}` from an attached client.
pub const T_RESIZE: u8 = 3;
/// `{"code":..}` when the attached session's process exits.
pub const T_EXIT: u8 = 4;

const MAX_FRAME: usize = 64 << 20;

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, t: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut head = [0u8; 5];
    head[0] = t;
    head[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&head).await?;
    w.write_all(payload).await?;
    w.flush().await
}

/// `Ok(None)` on clean EOF.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(head[1..].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other(format!("frame too large: {len}")));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(Some((head[0], buf)))
}

pub async fn write_json<W: AsyncWrite + Unpin>(w: &mut W, v: &serde_json::Value) -> std::io::Result<()> {
    write_frame(w, T_JSON, &serde_json::to_vec(v)?).await
}
