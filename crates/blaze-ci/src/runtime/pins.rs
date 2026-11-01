//! The pin connection: one long-lived connection to the daemon on which every checkout's root
//! is held open ([`Request::Open`]), so the daemon's GC keeps it, until it is closed
//! ([`Request::Close`]). The daemon counts opens per connection; so does this, to open each
//! root as often again after reconnecting. A root the daemon no longer has then is reported to
//! the service ([`Input::Unpinned`]).

use std::path::PathBuf;

use blaze_api::{
    Request, Response, ViewRef,
    client::{Client, ClientError},
};
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_vfs::View;

use super::{api_error, log, said_or};
use crate::service::Input;

pub(crate) enum Command {
    /// Opens `view` and holds its root; the root, or why not, goes to `reply` (for the
    /// service's own pins, `None`: a failure is [`Input::Unpinned`]).
    Open { view: ViewRef, reply: Option<crossbeam_channel::Sender<Result<ContentId, String>>> },
    /// Lets go of one hold of `root`; one this does not hold (lost when the daemon restarted) is
    /// already reported.
    Close { root: ContentId },
}

struct Pins {
    socket: PathBuf,
    client: Option<Client>,
    /// Sorted by root where it is walked, so roots are held again in that order.
    held: HashMap<ContentId, u32>,
    service: crossbeam_channel::Sender<Input>,
}

pub(crate) fn run(socket: PathBuf, commands: crossbeam_channel::Receiver<Command>, service: crossbeam_channel::Sender<Input>) {
    let mut pins = Pins { socket, client: None, held: HashMap::default(), service };
    for command in commands {
        match command {
            Command::Open { view, reply } => {
                let pinned = match &view {
                    ViewRef::Root(View { root }) => Some(*root),
                    _ => None,
                };
                let result = pins.open(view);
                match (reply, pinned, result) {
                    (Some(reply), _, result) => {
                        let _ = reply.send(result);
                    }
                    (None, Some(root), Err(why)) => {
                        let _ = pins.service.send(Input::Unpinned { root, why });
                    }
                    // alloc: error path
                    (None, _, Err(why)) => log(&format!("holding a view open failed: {why}")),
                    (None, _, Ok(_)) => {}
                }
            }
            Command::Close { root } => pins.close(root),
        }
    }
}

impl Pins {
    fn open(&mut self, view: ViewRef) -> Result<ContentId, String> {
        for attempt in 0..2 {
            let client = self.connect()?;
            match open(client, view.clone()) { // clone: once per pin; a root's view owns nothing
                Ok(Ok(root)) => {
                    *self.held.entry(root).or_default() += 1;
                    return Ok(root);
                }
                Ok(Err(why)) => return Err(why),
                Err(error) => {
                    self.client = None;
                    if attempt == 1 {
                        // alloc: error path
                        return Err(format!("the daemon at {}: {error}", self.socket.display()));
                    }
                }
            }
        }
        // alloc: error path
        Err(format!("the daemon at {} could not be asked", self.socket.display()))
    }

    fn close(&mut self, root: ContentId) {
        let Some(count) = self.held.get_mut(&root) else { return };
        *count -= 1;
        if *count == 0 {
            self.held.remove(&root);
        }
        // alloc: once per root let go, the daemon's name for an error
        let socket = self.socket.display().to_string();
        let result = self.connect().and_then(|client| {
            let mut said = Vec::new(); // alloc: none until the daemon says an error
            let ok = client
                .call(Request::Close { root }, |response| {
                    if let Response::Error(error) = response {
                        said.push(api_error(error));
                    }
                })
                .map_err(|error| format!("the daemon at {socket}: {error}"))?; // alloc: error path
            if ok { Ok(()) } else { Err(said_or(&said, "the daemon refused and said nothing more")) }
        });
        if let Err(why) = result {
            // A lost connection closed every hold already.
            log(&format!("letting go of root {root:x} failed: {why}")); // alloc: error path
        }
    }

    /// The connection, made again if it was lost, with every root held on it once more.
    fn connect(&mut self) -> Result<&mut Client, String> {
        if self.client.is_none() {
            let mut client = Client::unix(&self.socket).map_err(|error| {
                // alloc: error path
                format!("could not reach the daemon at {}: {error}", self.socket.display())
            })?;
            // alloc: once per connection lost, the roots held again
            let mut held: Vec<(ContentId, u32)> = self.held.iter().map(|(root, count)| (*root, *count)).collect();
            held.sort_unstable_by_key(|&(root, _)| root);
            for (root, count) in held {
                for _ in 0..count {
                    match open(&mut client, ViewRef::Root(View { root })) {
                        Ok(Ok(_)) => {}
                        Ok(Err(why)) => {
                            self.held.remove(&root);
                            let _ = self.service.send(Input::Unpinned { root, why });
                            break;
                        }
                        // The counts stand: the next connection holds them again.
                        Err(error) => {
                            return Err(format!( // alloc: error path
                                "the daemon at {} hung up while its roots were held again: {error}",
                                self.socket.display()
                            ));
                        }
                    }
                }
            }
            self.client = Some(client);
        }
        Ok(self.client.as_mut().expect("connected"))
    }
}

/// [`Request::Open`] on `client`: the root, or the daemon's refusal; the outer error is the
/// connection's.
pub(crate) fn open(client: &mut Client, view: ViewRef) -> Result<Result<ContentId, String>, ClientError> {
    let mut opened = None;
    let mut said = Vec::new(); // alloc: none until the daemon says an error
    let ok = client.call(Request::Open { view }, |response| match response {
        Response::Opened { root } => opened = Some(*root),
        Response::Error(error) => said.push(api_error(error)),
        _ => {}
    })?;
    Ok(match (ok, opened) {
        (true, Some(root)) => Ok(root),
        // alloc: error path
        (true, None) => Err("the daemon ended the open without a root".to_owned()),
        (false, _) => Err(said_or(&said, "the daemon refused the view and said nothing more")),
    })
}
