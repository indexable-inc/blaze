//! Zero-alloc typed linear filesystem watch requests and inert generational handles.

#![blaze::sans_io]

pub mod req;

pub use req::{
    NextChangeReq, SimWatchHandler, SimWatchHandlerError, SimWatchOpts, UnwatchReq, WatchAns,
    WatchChange, WatchChangeKind, WatchDirOpts, WatchDirReq, WatchError, WatchHandle, WatchReq,
};
