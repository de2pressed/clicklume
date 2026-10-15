//! Bounded concurrent IPC handling. A GUI subscription occupies one worker;
//! the remaining workers keep CLI commands responsive without spawning threads
//! for every health probe or allowing slow clients to exhaust process resources.

use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const WORKERS: usize = 4;
pub const QUEUED_CONNECTIONS: usize = 16;

pub struct ConnectionPool {
    tx: Option<SyncSender<(UnixStream, Instant)>>,
    workers: Vec<JoinHandle<()>>,
}

impl ConnectionPool {
    pub fn new(
        handler: impl Fn(UnixStream, Instant) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<(UnixStream, Instant)>(QUEUED_CONNECTIONS);
        let rx = Arc::new(Mutex::new(rx));
        let handler = Arc::new(handler);
        let mut pool = Self {
            tx: Some(tx),
            workers: Vec::new(),
        };
        for index in 0..WORKERS {
            let rx = rx.clone();
            let handler = handler.clone();
            pool.workers.push(
                thread::Builder::new()
                    .name(format!("clicklume-ipc-{index}"))
                    .spawn(move || loop {
                        let next = rx.lock().unwrap().recv();
                        match next {
                            Ok((stream, deadline)) if Instant::now() < deadline => {
                                handler(stream, deadline)
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    })?,
            );
        }
        Ok(pool)
    }

    pub fn submit(&self, stream: UnixStream) {
        // A full queue drops the new connection immediately. Never wait on
        // slow clients in the accept loop, and never allocate another thread.
        // Include queue time in the read deadline: a backlog of slow clients
        // must not multiply command latency into many one-second waits.
        let job = (stream, Instant::now() + Duration::from_secs(1));
        if let Err(error) = self.tx.as_ref().unwrap().try_send(job) {
            log::debug!("IPC capacity reached: {error}");
        }
    }
}

impl Drop for ConnectionPool {
    fn drop(&mut self) {
        self.tx.take();
        for worker in self.workers.drain(..) {
            if worker.join().is_err() {
                log::error!("IPC worker panicked");
            }
        }
    }
}
