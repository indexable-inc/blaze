//! Zero-alloc typed linear network requests (`blaze_core::request`) and typestate stream/socket
//! handles (`blaze_core::handle`).

#![blaze::sans_io]

pub use blaze_core::handle::{Conn, Listener, Socket};
pub use blaze_core::request::{
    Accept, BufError, ByteLen, CloseConn, Connect, ConnectOpts, DmaBuf, FilledBuf, Listen,
    ListenOpts, NetError, RecvInto, RecvIntoOpts, SendFrom, SendFromOpts, Unlisten,
};

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use blaze_core::{
        Address, FileName,
        handle::{ExitKind, TaskId},
        request::SimHandler,
        sim::{Faults, Sim},
    };
    use blaze_own::{testing::heap, with_brand};

    use super::{Connect, ConnectOpts, Listen, ListenOpts};
    use crate::{
        io::{Io, Limits, SimWorld},
        mem::TASK_FRAME,
    };

    #[test]
    fn cx_run_listen_connect_unlisten_over_sim_handler_reclaims_cleanly() {
        let h = heap();
        let io = Io::simulated(SimWorld::new(77, Limits::USUAL));
        let cx = io.cx(Duration::from_secs(10), TASK_FRAME).expect("root cx");
        let mut sim = Sim::new(77, Faults::default());
        let addr = Address::Unix(FileName::new("cx-run.sock").expect("valid socket path"));

        with_brand(|brand| {
            let mut handler = SimHandler::new(&mut sim, TaskId(1), brand, h.alloc());

            let listener = cx
                .run("net.listen", &mut handler, Listen::new(ListenOpts { addr: addr.clone() }))
                .expect("handler perform listen")
                .expect("listen ok");

            let client = cx
                .run("net.connect", &mut handler, Connect::new(ConnectOpts { addr }))
                .expect("handler perform connect")
                .expect("connect ok");

            let server = cx
                .run("net.accept", &mut handler, listener.accept())
                .expect("handler perform accept")
                .expect("accept ok");

            cx.run("net.close_client", &mut handler, client.close())
                .expect("handler perform close client")
                .expect("close client ok");
            cx.run("net.close_server", &mut handler, server.close())
                .expect("handler perform close server")
                .expect("close server ok");
            cx.run("net.unlisten", &mut handler, listener.unlisten())
                .expect("handler perform unlisten")
                .expect("unlisten ok");

            assert_eq!(handler.finish_task(ExitKind::Normal), Ok(0));
        });
    }
}
