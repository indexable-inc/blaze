use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Answers each line of each connection with `echo <line>`.
async fn serve(listener: TcpListener) {
    loop {
        let (socket, _) = listener.accept().await.unwrap();
        tokio::spawn(async move {
            let (read, mut write) = socket.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                write.write_all(format!("echo {line}\n").as_bytes()).await.unwrap();
            }
        });
    }
}

/// What the server on localhost answers each of `clients` clients, in order.
async fn round_trip(clients: usize) -> Vec<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener));
    let tasks: Vec<_> = (0..clients)
        .map(|i| {
            tokio::spawn(async move {
                let stream = TcpStream::connect(addr).await.unwrap();
                let (read, mut write) = stream.into_split();
                write.write_all(format!("client {i}\n").as_bytes()).await.unwrap();
                write.shutdown().await.unwrap();
                BufReader::new(read).lines().next_line().await.unwrap().unwrap()
            })
        })
        .collect();
    let mut answers = Vec::new();
    for task in tasks {
        answers.push(task.await.unwrap());
    }
    answers
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    for line in round_trip(4).await {
        println!("{line}");
    }
    let named = tokio::task::spawn_blocking(|| std::thread::current().name().is_some()).await.unwrap();
    println!("blocking thread named {named}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn echoes() {
        assert_eq!(round_trip(2).await, ["echo client 0", "echo client 1"]);
    }
}
