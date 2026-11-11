//! The client for a caller that is not Rust: a call is a method's name and its options in
//! JSON, and its answer is JSON. The JSON is the API's one rendering ([`blaze_api::json`]):
//! a request and a response are written as serde writes blaze-api's types, and the options
//! and outcomes are [`crate::ask`]'s. `blaze-client-c` and `blaze-napi` are thin over this.
//!
//! [`Calls`] is a state machine beside the [`Client`]: [`Calls::begin`] sends a call and
//! [`Calls::on`] takes each event the client tells, so any number of calls are under way on
//! the one connection.

use std::{path::PathBuf, time::Duration};

use blaze_api::{Request, Response};
use serde::Deserialize;

use crate::{
    Client, Connect, Error, Start,
    ask::{self, Answer, Call, Method, Options, WhyOptions},
    mux::{End, Event},
    place,
};

/// How a foreign caller connects ([`Connect`]).
#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct ConnectOptions {
    /// The daemon's home (`BLAZE_HOME`'s meaning); none for the environment's.
    #[serde(default)]
    pub home: Option<PathBuf>,
    /// The `blaze` program a daemon is started with.
    #[serde(default)]
    pub blaze: Option<PathBuf>,
    /// Whether a daemon is started when none listens; true when not said.
    #[serde(default)]
    pub start: Option<bool>,
    /// How long the connect may take, in milliseconds.
    pub within_ms: u64,
}

/// A `status`'s options.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
struct StatusOptions {
    within_ms: u64,
}

/// A whole request's options: the request as the API's JSON has it.
#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
struct RequestOptions {
    request: Request,
    within_ms: u64,
}

fn parsed<'a, T: Deserialize<'a>>(what: &str, json: &'a [u8]) -> Result<T, Error> {
    sonic_rs::from_slice(json)
        // alloc: error path, options that do not parse.
        .map_err(|error| Error::Asked(format!("the options of `{what}` are not its JSON: {error}")))
}

/// Connects as `json` (a [`ConnectOptions`]) says.
pub fn connect(json: &[u8]) -> Result<Client, Error> {
    let options: ConnectOptions = parsed("connect", json)?;
    Client::connect_with(&Connect {
        home: options.home.as_deref(),
        blaze: options.blaze.as_deref(),
        start: match options.start {
            Some(false) => Start::Never,
            Some(true) | None => Start::IfMissing,
        },
        within: Duration::from_millis(options.within_ms),
    })
}

/// What [`Calls::on`] made of an event.
#[derive(Debug)]
pub enum Told {
    /// A response of the call sent under `id`, to stream to the caller.
    Response { id: u64, response: Response },
    /// The call sent under `id` is over: its answer.
    Ended { id: u64, result: Result<Answer, Error> },
    /// An event of a request these calls did not send.
    Other(Event),
}

/// The calls under way on one client.
#[derive(Debug, Default)]
pub struct Calls {
    open: Vec<(u64, Call)>,
}

impl Calls {
    pub fn new() -> Calls {
        Calls::default()
    }

    /// Sends the call `method` (a [`Method`]'s name) with the options `json`: the id its
    /// responses and its end are told under. `build`, `check` and `test` take an [`Options`],
    /// `why` a [`WhyOptions`], `status` `{"within_ms": N}`, and `request`
    /// `{"request": <a Request>, "within_ms": N}`.
    pub fn begin(&mut self, client: &mut Client, method: &str, json: &[u8]) -> Result<u64, Error> {
        let Some(method) = Method::named(method) else {
            let names = Method::ALL.map(Method::name).join(", ");
            // alloc: error path, a method that is none.
            return Err(Error::Asked(format!("`{method}` is no call of the client: {names}")));
        };
        let (request, within_ms) = match method {
            Method::Build | Method::Check | Method::Test => {
                let options: Options = parsed(method.name(), json)?;
                let env = place::env_of(options.env.as_deref()).map_err(Error::Asked)?;
                (ask::request(method, &options, &env).map_err(Error::Asked)?, options.within_ms)
            }
            Method::Why => {
                let options: WhyOptions = parsed(method.name(), json)?;
                (ask::why(&options), options.within_ms)
            }
            Method::Status => {
                let options: StatusOptions = parsed(method.name(), json)?;
                (Request::Status, options.within_ms)
            }
            Method::Request => {
                let options: RequestOptions = parsed(method.name(), json)?;
                (options.request, options.within_ms)
            }
        };
        self.send(client, method, &request, Duration::from_millis(within_ms))
    }

    /// Sends `request` whole, to end within `within`; its answer is its `Done`.
    pub fn begin_request(
        &mut self,
        client: &mut Client,
        request: &Request,
        within: Duration,
    ) -> Result<u64, Error> {
        self.send(client, Method::Request, request, within)
    }

    fn send(
        &mut self,
        client: &mut Client,
        method: Method,
        request: &Request,
        within: Duration,
    ) -> Result<u64, Error> {
        let id = client.send(request, within)?;
        self.open.push((id, Call::new(method)));
        Ok(id)
    }

    /// Takes an event the client told.
    pub fn on(&mut self, event: Event) -> Told {
        let (Event::Response { id, .. } | Event::Ended { id, .. }) = &event;
        let Some(at) = self.open.iter().position(|(it, _)| it == id) else {
            return Told::Other(event);
        };
        match event {
            Event::Response { id, response } => {
                // clone: the call keeps what its answer holds, the caller streams the whole.
                self.open[at].1.take(response.clone());
                Told::Response { id, response }
            }
            Event::Ended { id, how } => {
                let (_, call) = self.open.swap_remove(at);
                let result = match how {
                    End::Done { ok } => call.finish(ok),
                    End::Failed(fault) => Err(Error::Fault(fault)),
                };
                Told::Ended { id, result }
            }
        }
    }

    /// How many calls are under way.
    pub fn open(&self) -> usize {
        self.open.len()
    }
}

/// One call, blocking, for a caller with one thread: each response to `on_event`, then the
/// answer. `request` says what [`Calls::begin`] or [`Calls::begin_request`] does.
pub fn call(
    client: &mut Client,
    begin: impl FnOnce(&mut Calls, &mut Client) -> Result<u64, Error>,
    mut on_event: impl FnMut(u64, &Response),
) -> Result<Answer, Error> {
    let mut calls = Calls::new();
    let id = begin(&mut calls, client)?;
    let _span =
        tracing::info_span!(target: "blaze", "client: wait for the daemon's answer to a call", id)
            .entered();
    // Bounded: the call ends by its deadline, which the mux holds it to.
    loop {
        // The wait is to the call's own deadline, the mux's.
        let crate::Next::Event(event) = client.next(Duration::MAX)? else { continue };
        match calls.on(event) {
            Told::Response { id, response } => on_event(id, &response),
            Told::Ended { result, .. } => return result,
            Told::Other(_) => {}
        }
    }
}
