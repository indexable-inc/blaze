//! Actors: each one a thread that runs only while the explorer has handed it the turn.
//!
//! An actor (a writer, a reader, a kernel get) runs real blaze code on its own thread, with
//! its own `Writer` and guard, as a daemon's worker does. It stops at each schedule point
//! ([`Turn::pause`], and each request to the store's owner) and waits until the controller
//! gives it the turn again, so exactly one thread runs at a time and an execution is a
//! function of the explorer's choices alone. A schedule point inside a library call (the
//! kernel's store calls go through a [`Turn`]) is how the explorer interleaves two writers in
//! the middle of one `Kernel::get`.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::thread::JoinHandle;

use blaze_cas::{Key, Ticket};
use crossbeam_channel::{Receiver, Sender};

use crate::explore::{Failure, panic_text};
use crate::properties;

/// How the controller hands an actor its turn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resume {
    Go,
    /// Its wait ended: what it waited on is durable.
    Durable,
    /// Its wait ended: the store stopped, so it never will be.
    Broken,
}

enum Event {
    Pause(String),
    Flush,
    Await(Vec<Ticket>),
    Answer(Vec<Key>),
    Done,
    Failed(Failure),
    Panicked(String),
}

/// What an actor does next, once it has the turn.
#[derive(Clone, Debug)]
pub enum Next {
    Step(String),
    /// Asks the store's owner to flush.
    Flush,
    /// Waits until each is durable, or the store has stopped.
    Await(Vec<Ticket>),
    /// Answers, with the keys of what it kept.
    Answer(Vec<Key>),
    Done,
}

/// The unwind an actor makes when the controller has gone: the execution ended.
struct Stopped;

/// An actor's side of the turn.
pub struct Turn {
    id: usize,
    go: Receiver<Resume>,
    events: Sender<(usize, Event)>,
}

impl Turn {
    fn send(&self, event: Event) {
        if self.events.send((self.id, event)).is_err() {
            resume_unwind(Box::new(Stopped));
        }
    }

    fn wait(&self) -> Resume {
        match self.go.recv() {
            Ok(resume) => resume,
            Err(_) => resume_unwind(Box::new(Stopped)),
        }
    }

    /// A schedule point: the explorer may run any other actor, or any IO, before this one goes
    /// on to do `label`.
    pub fn pause(&self, label: impl Into<String>) {
        self.send(Event::Pause(label.into()));
        self.wait();
    }

    /// Asks the store's owner to flush, as a build asks the daemon.
    pub fn flush(&self) {
        self.send(Event::Flush);
        self.wait();
    }

    /// Waits until every ticket is durable (`Resume::Durable`) or the store stops
    /// (`Resume::Broken`).
    pub fn await_durable(&self, tickets: Vec<Ticket>) -> Resume {
        self.send(Event::Await(tickets));
        self.wait()
    }

    /// Answers the build, having kept `keys`.
    pub fn answer(&self, keys: Vec<Key>) {
        self.send(Event::Answer(keys));
        self.wait();
    }

    /// Ends the execution with `failure`.
    pub fn fail(&self, failure: Failure) -> ! {
        self.send(Event::Failed(failure));
        resume_unwind(Box::new(Stopped))
    }
}

struct Actor {
    name: String,
    go: Option<Sender<Resume>>,
    next: Next,
    thread: Option<JoinHandle<()>>,
}

/// The controller's side: every actor of one execution.
pub struct Actors {
    actors: Vec<Actor>,
    events: Receiver<(usize, Event)>,
    send: Sender<(usize, Event)>,
}

impl Default for Actors {
    fn default() -> Actors {
        let (send, events) = crossbeam_channel::unbounded();
        Actors { actors: Vec::new(), events, send }
    }
}

impl Actors {
    /// Starts `body` as the actor `name`; it runs until its first schedule point.
    pub fn spawn(
        &mut self,
        name: &str,
        body: impl FnOnce(&Turn) + Send + 'static,
    ) -> Result<(), Failure> {
        let (go, wait) = crossbeam_channel::unbounded();
        let events = self.send.clone();
        let id = self.actors.len();
        let thread = std::thread::Builder::new()
            .name(format!("storemodel {name}"))
            .spawn(move || {
                let turn = Turn { id, go: wait, events };
                match catch_unwind(AssertUnwindSafe(|| body(&turn))) {
                    // A controller that has gone needs no word of the end.
                    Ok(()) => drop(turn.events.send((id, Event::Done))),
                    Err(payload) if payload.is::<Stopped>() => {}
                    Err(payload) => {
                        drop(turn.events.send((id, Event::Panicked(panic_text(&*payload)))))
                    }
                }
            })
            .unwrap_or_else(|why| {
                panic!("storemodel: the thread of {name} could not start: {why}")
            });
        self.actors.push(Actor {
            name: name.to_owned(),
            go: Some(go),
            next: Next::Done,
            thread: Some(thread),
        });
        self.next_event(id)
    }

    fn next_event(&mut self, id: usize) -> Result<(), Failure> {
        let (from, event) =
            self.events.recv().expect("an actor that has the turn sends its next event");
        assert_eq!(from, id, "only the actor with the turn runs");
        let name = &self.actors[id].name;
        self.actors[id].next = match event {
            Event::Pause(label) => Next::Step(label),
            Event::Flush => Next::Flush,
            Event::Await(tickets) => Next::Await(tickets),
            Event::Answer(keys) => Next::Answer(keys),
            Event::Done => Next::Done,
            Event::Failed(failure) => return Err(failure),
            Event::Panicked(text) => {
                return Err(Failure::new(properties::NO_PANIC, format!("{name} panicked: {text}")));
            }
        };
        Ok(())
    }

    /// Hands actor `id` the turn, and waits until it reaches its next schedule point.
    pub fn resume(&mut self, id: usize, how: Resume) -> Result<(), Failure> {
        let go = self.actors[id].go.as_ref().expect("an actor still running");
        go.send(how).expect("an actor waiting for its turn");
        self.next_event(id)
    }

    pub fn len(&self) -> usize {
        self.actors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.actors.is_empty()
    }

    pub fn name(&self, id: usize) -> &str {
        &self.actors[id].name
    }

    pub fn next(&self, id: usize) -> &Next {
        &self.actors[id].next
    }
}

impl Drop for Actors {
    fn drop(&mut self) {
        // An actor still waiting finds its channel closed and unwinds.
        for actor in &mut self.actors {
            actor.go = None;
        }
        for actor in &mut self.actors {
            if let Some(thread) = actor.thread.take() {
                // Its body's panics were caught and sent as events.
                drop(thread.join());
            }
        }
    }
}
