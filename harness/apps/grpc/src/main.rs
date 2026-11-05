//! A tonic server and client. `grpc serve <port>` is the server; `grpc client <port> <n>` is
//! the client, which makes every kind of call and prints what came back.

use std::{collections::BTreeMap, pin::Pin, sync::Arc};

use tokio::sync::Mutex;
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use tonic::{transport::Server, Code, Request, Response, Status, Streaming};

pub mod ledger {
    tonic::include_proto!("ledger");
}

use ledger::{
    entry::Source,
    ledger_client::LedgerClient,
    ledger_server::{Ledger, LedgerServer},
    Account, Balance, Entry,
};

#[derive(Default)]
struct Books {
    accounts: Mutex<BTreeMap<String, Vec<Entry>>>,
}

impl Books {
    async fn record(&self, entry: Entry) -> Result<Balance, Status> {
        if entry.account.is_empty() {
            return Err(Status::invalid_argument("an entry names its account"));
        }
        let mut accounts = self.accounts.lock().await;
        let entries = accounts.entry(entry.account.clone()).or_default();
        let total: i64 = entries.iter().map(|it| it.amount).sum::<i64>() + entry.amount;
        if total < 0 {
            return Err(Status::failed_precondition(format!("{} would be overdrawn by {}", entry.account, -total)));
        }
        let account = entry.account.clone();
        entries.push(entry);
        Ok(Balance { account, total, entries: entries.len() as u32 })
    }
}

type Reply<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

#[tonic::async_trait]
impl Ledger for Arc<Books> {
    async fn deposit(&self, request: Request<Entry>) -> Result<Response<Balance>, Status> {
        self.record(request.into_inner()).await.map(Response::new)
    }

    type StatementStream = Reply<Entry>;

    async fn statement(&self, request: Request<Account>) -> Result<Response<Self::StatementStream>, Status> {
        let name = request.into_inner().name;
        let entries = self.accounts.lock().await.get(&name).cloned();
        let entries = entries.ok_or_else(|| Status::not_found(format!("no account {name}")))?;
        Ok(Response::new(Box::pin(tokio_stream::iter(entries.into_iter().map(Ok)))))
    }

    async fn batch(&self, request: Request<Streaming<Entry>>) -> Result<Response<Balance>, Status> {
        let mut stream = request.into_inner();
        let mut last = None;
        while let Some(entry) = stream.next().await {
            last = Some(self.record(entry?).await?);
        }
        last.map(Response::new).ok_or_else(|| Status::invalid_argument("an empty batch"))
    }

    type ExchangeStream = Reply<Balance>;

    async fn exchange(&self, request: Request<Streaming<Entry>>) -> Result<Response<Self::ExchangeStream>, Status> {
        let mut stream = request.into_inner();
        let books = self.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
            while let Some(entry) = stream.next().await {
                let balance = match entry {
                    Ok(entry) => books.record(entry).await,
                    Err(status) => Err(status),
                };
                if sender.send(balance).await.is_err() {
                    return;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

fn entry(account: &str, n: i64) -> Entry {
    Entry {
        account: account.to_owned(),
        amount: n * 37 % 101 - 20,
        memo: format!("entry {n} \u{2713}"),
        tags: (0..n % 4).map(|tag| format!("t{tag}")).collect(),
        source: Some(if n % 2 == 0 { Source::Teller(format!("teller {}", n % 3)) } else { Source::Machine(n as u32) }),
    }
}

fn shown<T: std::fmt::Debug>(result: Result<T, Status>) -> String {
    match result {
        Ok(value) => format!("{value:?}"),
        Err(status) => format!("{:?}: {}", status.code(), status.message()),
    }
}

async fn client(port: u16, count: i64) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = LedgerClient::connect(format!("http://127.0.0.1:{port}")).await?;
    client.deposit(entry("opening", 1000)).await.ok();
    for n in 0..count {
        let reply = client.deposit(entry("alice", n)).await.map(Response::into_inner);
        if n < 12 || reply.is_err() {
            println!("deposit {n}: {}", shown(reply));
        }
    }
    println!("deposit to nobody: {}", shown(client.deposit(entry("", 1)).await.map(Response::into_inner)));
    let mut statement = client.statement(Account { name: "alice".to_owned() }).await?.into_inner();
    let (mut entries, mut sum, mut machines) = (0, 0, 0);
    while let Some(entry) = statement.message().await? {
        entries += 1;
        sum += entry.amount;
        machines += u32::from(matches!(entry.source, Some(Source::Machine(_))));
    }
    println!("statement: {entries} entries, sum {sum}, {machines} from machines");
    let missing = client.statement(Account { name: "nobody".to_owned() }).await.map(|_| ());
    println!("statement of nobody: {}", shown(missing));
    let batch = tokio_stream::iter((0..count).map(|n| entry("bob", n + 30)));
    println!("batch: {}", shown(client.batch(batch).await.map(Response::into_inner)));
    let exchange = tokio_stream::iter((0..40).map(|n| entry(if n % 2 == 0 { "carol" } else { "dave" }, n * 3)));
    let mut balances = client.exchange(exchange).await?.into_inner();
    let (mut ok, mut refused, mut last) = (0, 0, None);
    while let Some(balance) = balances.next().await {
        match balance {
            Ok(balance) => {
                ok += 1;
                last = Some(balance);
            }
            Err(status) if status.code() == Code::FailedPrecondition => refused += 1,
            Err(status) => {
                println!("exchange ended: {:?}", status.code());
                break;
            }
        }
    }
    println!("exchange: {ok} recorded, {refused} refused, last {last:?}");
    let mut calls = tokio::task::JoinSet::new();
    for worker in 0..16i64 {
        let mut client = client.clone();
        calls.spawn(async move {
            let mut total = 0;
            for n in 0..count {
                if let Ok(balance) = client.deposit(entry(&format!("worker{worker}"), n + worker)).await {
                    total = balance.into_inner().total;
                }
            }
            (worker, total)
        });
    }
    let mut totals = Vec::new();
    while let Some(done) = calls.join_next().await {
        totals.push(done?);
    }
    totals.sort();
    println!("16 clients at once: {totals:?}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let port: u16 = args.get(1).ok_or("usage: grpc serve <port> | grpc client <port> <calls>")?.parse()?;
    match args[0].as_str() {
        "serve" => {
            let books = Arc::new(Books::default());
            Server::builder().add_service(LedgerServer::new(books)).serve(([127, 0, 0, 1], port).into()).await?;
        }
        _ => client(port, args.get(2).and_then(|it| it.parse().ok()).unwrap_or(50)).await?,
    }
    Ok(())
}
