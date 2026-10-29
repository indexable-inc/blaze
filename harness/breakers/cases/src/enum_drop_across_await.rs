//! Two drops in futures where blaze kept a moved-out value alive across a suspension point:
//! reading the discriminant of an enum whose every variant drops the same way (the
//! otherwise edge included), and testing a drop flag whose set and unset edges go to the
//! same block. Reduced from uv and ty by Charlie Marsh (astral-experiments/blaze).

use std::future::Future;
use std::pin::{Pin, pin};
use std::task::{Context, Poll, Waker};

struct Twice(bool);
impl Future for Twice {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        if std::mem::replace(&mut self.0, true) { Poll::Ready(()) } else { Poll::Pending }
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        std::hint::black_box([u64::MAX; 64]);
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

struct Packet {
    payload: Option<String>,
    bias: u8,
}

async fn partial_packet(packet: Packet) -> u64 {
    let payload = if let Some(text) = packet.payload { text } else { String::new() };
    let sum = payload.len() as u64 + u64::from(packet.bias);
    drop(payload);
    Twice(false).await;
    sum
}

/// The early return can suspend before `?` produces a value to drop.
async fn take_or_collect(owned: String, tag: u8) -> Result<(String, Vec<u8>), String> {
    let bytes = match tag {
        0 => return std::future::ready(Ok((owned, vec![3]))).await,
        1 => std::future::ready(Ok(owned.as_bytes().to_vec())).await,
        _ => Err(String::new()),
    }?;
    Ok((owned, bytes))
}

pub fn run() -> (u64, Vec<Result<(String, Vec<u8>), String>>) {
    let packet = Packet { payload: Some(String::from("abc")), bias: 2 };
    let taken = (0..3).map(|tag| block_on(take_or_collect(String::from("abc"), tag))).collect();
    (block_on(partial_packet(packet)), taken)
}
