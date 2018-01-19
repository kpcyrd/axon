// Copyright (C) 2017  ParadoxSpiral
//
// This file is part of axon.
//
// Axon is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// Axon is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with Axon.  If not, see <http://www.gnu.org/licenses/>.

use futures::{Future, Sink, Stream as FutStream};
use futures::future::{self, Either};
use futures::sink::Wait;
use futures::stream::{SplitSink, SplitStream};
use futures::sync::mpsc::{self, Receiver, Sender};
use parking_lot::Mutex;
use serde_json;
use synapse_rpc;
use synapse_rpc::message::{CMessage, SMessage};
use tokio::reactor::{Core, Timeout};
use url::Url;
use websocket::ClientBuilder;
use websocket::async::{MessageCodec, Stream};
use websocket::async::client::Framed;
use websocket::message::OwnedMessage;

use std::cell::RefCell;
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use view::View;

type InnerStream = Framed<Box<Stream + Send>, MessageCodec<OwnedMessage>>;
type SplitSocket = (
    RefCell<SplitStream<InnerStream>>,
    Mutex<Wait<SplitSink<InnerStream>>>,
);

enum StreamRes {
    Close,
    Msg(OwnedMessage),
}

pub struct RpcContext<'v> {
    socket: RefCell<Option<SplitSocket>>,
    waiter: (RefCell<Sender<()>>, RefCell<Receiver<()>>),
    // FIXME: Once feature `integer atomics` lands, switch to AtomicU64
    serial: AtomicUsize,
    core: RefCell<Core>,
    view: &'v View,
}

unsafe impl<'v> Send for RpcContext<'v> {}
unsafe impl<'v> Sync for RpcContext<'v> {}

impl<'v> RpcContext<'v> {
    pub fn new(view: &'v View) -> RpcContext<'v> {
        RpcContext {
            socket: RefCell::new(None),
            waiter: {
                let (s, r) = mpsc::channel(1);
                (RefCell::new(s), RefCell::new(r))
            },
            serial: AtomicUsize::new(0),
            core: RefCell::new(Core::new().unwrap()),
            view,
        }
    }

    pub fn init(&self, mut srv: Url, pass: &str) -> Result<(), String> {
        let url = srv.query_pairs_mut().append_pair("password", pass).finish();
        let (sink, mut stream) = {
            let mut core = self.core.borrow_mut();
            let timeout = Timeout::new(Duration::from_secs(10), &core.handle()).unwrap();
            let fut = ClientBuilder::new(url.as_str())
                .map_err(|err| format!("{}", err))?
                .async_connect(None, &core.handle())
                .map_err(|err| format!("{:?}", err))
                .select2(timeout.map(|_| "Timeout while connecting to server (10s)".to_owned()));
            match core.run(fut) {
                Ok(Either::A(((client, _), _))) => client.split(),
                Ok(Either::B((err, _))) | Err(Either::A((err, _))) => {
                    return Err(err);
                }
                _ => unreachable!(),
            }
        };

        if let OwnedMessage::Text(msg) = stream
            .by_ref()
            .wait()
            .next()
            .unwrap()
            .map_err(|err| format!("{:?}", err))?
        {
            let srv_ver = serde_json::from_str::<synapse_rpc::message::Version>(&msg)
                .map_err(|err| format!("{:?}", err))?;
            if srv_ver.major != synapse_rpc::MAJOR_VERSION {
                return Err(format!(
                    "Server version {:?} incompatible with client {}.{}",
                    srv_ver,
                    synapse_rpc::MAJOR_VERSION,
                    synapse_rpc::MINOR_VERSION
                ));
            }
        } else {
            return Err("Server sent non-text response, i.e. not its version".to_owned());
        }

        *self.socket.borrow_mut() = Some((RefCell::new(stream), Mutex::new(sink.wait())));
        self.wake();
        Ok(())
    }

    pub fn wake(&self) {
        self.waiter.0.borrow_mut().try_send(()).unwrap();
    }

    pub fn next_serial(&self) -> u64 {
        self.serial.fetch_add(1, Ordering::AcqRel) as _
    }

    pub fn send(&self, msg: CMessage) {
        match serde_json::to_string(&msg) {
            Err(e) => self.view.global_err(format!("{}", e.description())),
            Ok(msg) => self.send_raw(OwnedMessage::Text(msg)),
        }
    }

    fn send_raw(&self, msg: OwnedMessage) {
        let sink = self.socket.borrow();
        let sink = sink.as_ref();
        let mut sink = sink.unwrap().1.lock();

        match (sink.send(msg), sink.flush()) {
            (Err(e), _) | (_, Err(e)) => self.view.global_err(format!("{:?}", e)),
            _ => {}
        }
    }

    pub fn recv_until_death(&self) {
        // Each iteration represents the lifetime of a connection to a server
        loop {
            // Wait for initialization
            let mut waiter = self.waiter.1.borrow_mut();
            let _ = waiter.by_ref().wait().next().unwrap();

            // Check if exited before login
            let socket = self.socket.borrow();
            if socket.is_none() {
                return;
            }

            let mut core = self.core.borrow_mut();
            let socket = socket.as_ref().unwrap();
            let mut stream = socket.0.borrow_mut();

            let msg_handler = stream
                .by_ref()
                .map(|msg| StreamRes::Msg(msg))
                .map_err(|err| format!("{:?}", err))
                .select(
                    waiter
                        .by_ref()
                        .map(|_| StreamRes::Close)
                        .map_err(|err| format!("{:?}", err)),
                )
                .or_else(|e| future::err(self.view.global_err(e)))
                .and_then(|res| match res {
                    StreamRes::Msg(msg) => match msg {
                        OwnedMessage::Ping(p) => {
                            self.send_raw(OwnedMessage::Pong(p));
                            future::ok(())
                        }
                        OwnedMessage::Close(data) => {
                            self.view.server_close(data);
                            future::err(())
                        }
                        OwnedMessage::Text(s) => {
                            match serde_json::from_str::<SMessage>(&s) {
                                Err(e) => self.view.global_err(format!("{}", e.description())),
                                Ok(msg) => if let SMessage::ResourcesExtant { ref ids, .. } = msg {
                                    let ids: Vec<_> =
                                        ids.iter().map(|id| id.clone().into_owned()).collect();

                                    self.send(CMessage::Subscribe {
                                        serial: self.next_serial(),
                                        ids: ids.clone(),
                                    });
                                } else if let SMessage::ResourcesRemoved { ref ids, .. } = msg {
                                    self.send(CMessage::Unsubscribe {
                                        serial: self.next_serial(),
                                        ids: ids.clone(),
                                    });

                                    self.view.handle_rpc(self, &msg);
                                } else {
                                    self.view.handle_rpc(self, &msg);
                                },
                            };
                            future::ok(())
                        }
                        _ => unreachable!(),
                    },
                    StreamRes::Close => future::err(()),
                });

            // Wait until the stream is, or should be, terminated
            let _ = core.run(msg_handler.for_each(|_| Ok(())));

            if ::RUNNING.load(Ordering::Acquire) {
                *self.socket.borrow_mut() = None;
                continue;
            } else {
                break;
            }
        }
    }
}
