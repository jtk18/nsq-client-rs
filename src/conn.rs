// MIT License
// 
// Copyright (c) 2019-2021 Alessandro Cresto Miseroglio <alex179ohm@gmail.com>
// Copyright (c) 2019-2021 Tangram Technologies S.R.L. <https://tngrm.io>
// 
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
// 
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
// 
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use std::io;
use std::any::{Any, TypeId};

use actix::actors::resolver::{Connect, Resolver};
use actix::prelude::*;
use backoff::backoff::Backoff;
use backoff::ExponentialBackoff;
use log::{error, info};
use serde_json;
use tokio_codec::FramedRead;
use tokio_io::io::WriteHalf;
use tokio_io::AsyncRead;
use tokio_tcp::TcpStream;
use futures::stream::once;
use fnv::FnvHashMap;

use crate::codec::{NsqCodec, Cmd};
use crate::commands::{identify, nop, rdy, sub, fin, VERSION};
use crate::config::{Config, NsqdConfig};
use crate::error::Error;
use crate::msgs::{
    Auth, Sub, Ready, Cls,
    Resume, NsqBackoff, Fin, Msg,
    NsqMsg, AddHandler, InFlight};
//use crate::consumer_srvc::ConsumerService;

#[derive(Message, Clone)]
pub struct TcpConnect(pub String);

#[derive(Debug, PartialEq)]
pub enum ConnState {
    Neg,
    Auth,
    Sub,
    Ready,
    Started,
    Backoff,
    Resume,
    Stopped,
}

pub struct Connection
{
    addr: String,
    handlers: Vec<Box<Any>>,
    info_handler: Box<Any>,
    info_hashmap: FnvHashMap<TypeId, Box<Any>>,
    topic: String,
    channel: String,
    config: Config,
    tcp_backoff: ExponentialBackoff,
    backoff: ExponentialBackoff,
    cell: Option<actix::io::FramedWrite<WriteHalf<TcpStream>, NsqCodec>>,
    state: ConnState,
    rdy: u32,
    in_flight: u32,
    handler_ready: usize,
}

impl Default for Connection
{
    fn default() -> Connection {
        Connection {
            handlers: Vec::new(),
            info_handler: Box::new(()),
            info_hashmap: FnvHashMap::default(),
            topic: String::new(),
            channel: String::new(),
            config: Config::default(),
            tcp_backoff: ExponentialBackoff::default(),
            backoff: ExponentialBackoff::default(),
            cell: None,
            state: ConnState::Neg,
            addr: String::new(),
            rdy: 1,
            in_flight: 0,
            handler_ready: 0,
        }
    }
}

impl Connection
{
    pub fn new<S: Into<String>>(
        topic: S,
        channel: S,
        addr: S,
        config: Option<Config>,
        secret: Option<String>,
        rdy: Option<u32>) -> Connection
    {
        let mut tcp_backoff = ExponentialBackoff::default();
        let backoff = ExponentialBackoff::default();
        let cfg = match config {
            Some(cfg) => cfg,
            None => Config::default(),
        };
        let rdy = match rdy {
            Some(r) => r,
            None => 1,
        };
        tcp_backoff.max_elapsed_time = None;
        Connection {
            config: cfg,
            tcp_backoff,
            backoff,
            cell: None,
            topic: topic.into(),
            channel: channel.into(),
            state: ConnState::Neg,
            handlers: Vec::new(),
            info_handler: Box::new(()),
            info_hashmap: FnvHashMap::default(),
            addr: addr.into(),
            rdy: rdy,
            in_flight: 0,
            handler_ready: 0,
        }
    }
}

impl Actor for Connection
{
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Context<Self>) {
        info!("trying to connect [{}]", self.addr);
        self.handler_ready = self.handlers.len();
        ctx.add_message_stream(once(Ok(TcpConnect(self.addr.to_owned()))));
    }
}

//impl Connection {
//    fn add_inflight_handler(&mut self, handler: Recipient<InFlight>) {
//        self.info_hashmap.insert(TypeId::of::<InFlight>(), Box::new(handler));
//    }
//}
//
impl actix::io::WriteHandler<io::Error> for Connection
{
    fn error(&mut self, err: io::Error, _: &mut Self::Context) -> Running {
        error!("nsqd connection dropped: {}", err);
        Running::Stop
    }
}

// TODO: implement error
impl StreamHandler<Cmd, Error> for Connection
{

    fn finished(&mut self, ctx: &mut Self::Context) {
        error!("Nsqd connection dropped");
        ctx.stop();
    }

    fn error(&mut self, err: Error, _ctx: &mut Self::Context) -> Running {
        error!("Something goes wrong decoding message: {}", err);
        Running::Stop
    }

    fn handle(&mut self, msg: Cmd, ctx: &mut Self::Context)
    {
        match msg {
            Cmd::Heartbeat => {
                if let Some(ref mut cell) = self.cell {
                    cell.write(nop());
                } else {
                    error!("Nsqd connection dropped. trying reconnecting");
                    ctx.stop();
                }
            }
            Cmd::Response(s) => {
                match self.state {
                    ConnState::Neg => {
                        info!("trying negotiation [{}]", self.addr);
                        let config: NsqdConfig = match serde_json::from_str(s.as_str()) {
                            Ok(s) => { s },
                            Err(err) => {
                                error!("Negotiating json response invalid: {:?}", err);
                                return ctx.stop();
                            }
                        };
                        info!("configuration [{}] {:#?}", self.addr, config);
                        if config.auth_required {
                            info!("trying authentication [{}]", self.addr);
                            ctx.notify(Auth);
                        } else {
                            info!("subscribing [{}] topic: {} channel: {}", self.addr, self.topic, self.channel);
                            ctx.notify(Sub);
                        }
                    },
                    ConnState::Sub => {
                        ctx.notify(Sub);
                    },
                    ConnState::Ready => {
                        ctx.notify(Ready(self.rdy));
                    }
                    _ => {},
                }
            }
            // TODO: implement msg_queue and tumable RDY for fast processing multiple msgs
            Cmd::ResponseMsg(msgs) => {
                //let mut count = self.rdy;
                for (timestamp, attemps, id, body) in msgs {
                    if self.handler_ready > 0 { self.handler_ready -= 1 };
                    if let Some(handler) = self.handlers.get(self.handler_ready) {
                        if let Some(rec) = handler.downcast_ref::<Recipient<Msg>>() {
                            match rec.do_send(Msg{
                                timestamp, attemps, id, body,
                            }) {
                                Ok(_s) => {
                                    self.in_flight += 1;
                                    if let Some(info) = self.info_handler.downcast_ref::<Recipient<InFlight>>() {
                                        let _ = info.do_send(InFlight(self.in_flight));
                                    }
                                },
                                Err(e) => { error!("error sending msg to reader: {}", e) }
                            }

                        }
                    }
                    if self.handler_ready == 0 { self.handler_ready = self.handlers.len() }
                }
            },
            Cmd::ResponseError(s) => {
                error!("failed: {}", s);
            }
            Cmd::Command(_) => {
                if let Some(ref mut cell) = self.cell {
                    cell.write(rdy(1));
                }
            }
            _ => {},
        }
    }
}

impl Handler<TcpConnect> for Connection
{
    type Result=();
    fn handle(&mut self, msg:TcpConnect, ctx: &mut Self::Context) {
        Resolver::from_registry()
            .send(Connect::host(msg.0.as_str()))
            .into_actor(self)
            .map(move |res, act, ctx| match res {
                Ok(stream) => {
                    info!("connected [{}]", msg.0);
                    //stream.set_recv_buffer_size(act.config.output_buffer_size as usize);

                    let (r, w) = stream.split();

                    // configure write side of the connection
                    let mut framed =
                        actix::io::FramedWrite::new(w, NsqCodec{}, ctx);
                    let mut rx = FramedRead::new(r, NsqCodec{});
                    framed.write(Cmd::Magic(VERSION));
                    // send configuration to nsqd
                    let json = match serde_json::to_string(&act.config) {
                        Ok(s) => s,
                        Err(e) => {
                            error!("config cannot be formatted as json string: {}", e);
                            return ctx.stop();
                        }
                    };
                    // read connection
                    ctx.add_stream(rx);
                    framed.write(identify(json));
                    act.cell = Some(framed);

                    act.backoff.reset();
                    act.state = ConnState::Neg;
                }
                Err(err) => {
                    error!("can not connect [{}]", err);
                    // re-connect with backoff time.
                    // we stop current context, supervisor will restart it.
                    if let Some(timeout) = act.tcp_backoff.next_backoff() {
                        ctx.run_later(timeout, |_, ctx| ctx.stop());
                    }
                }
            })
            .map_err(|err, act, ctx| {
                error!("can not connect [{}]", err);
                // re-connect with backoff time.
                // we stop current context, supervisor will restart it.
                if let Some(timeout) = act.tcp_backoff.next_backoff() {
                    ctx.run_later(timeout, |_, ctx| ctx.stop());
                }
            })
            .wait(ctx);
    }
}

impl Handler<Cls> for Connection {
    type Result=();
    fn handle(&mut self, _msg: Cls, ctx: &mut Self::Context) {
        self.state = ConnState::Stopped;
        ctx.stop();
    }
}

impl Handler<Fin> for Connection
{
    type Result = ();
    fn handle(&mut self, msg: Fin, _ctx: &mut Self::Context) {
        // discard the in_flight messages
        if let Some(ref mut cell) = self.cell {
            cell.write(fin(&msg.0));
        }
        self.in_flight -= 1;
        if let Some(info) = self.info_handler.downcast_ref::<Recipient<InFlight>>() {
            let _ = info.do_send(InFlight(self.in_flight));
        }
    }
}

impl Handler<Ready> for Connection
{
    type Result = ();

    fn handle(&mut self, msg: Ready, _ctx: &mut Self::Context) {
        if let Some(ref mut cell) = self.cell {
            cell.write(rdy(msg.0));
        }
        if self.state == ConnState::Started {
            self.rdy = msg.0;
            info!("rdy updated [{}]", self.addr);

        } else { self.state = ConnState::Started; info!("Ready to go [{}] RDY: {}", self.addr, msg.0); }
    }
}


impl Handler<Auth> for Connection
{
    type Result = ();
    fn handle(&mut self, _msg: Auth, ctx: &mut Self::Context) {
        if let Some(ref mut cell) = self.cell {
            cell.write(sub(&self.topic, &self.channel));
        } else {
            error!("unable to identify: connection dropped [{}]", self.addr);
            ctx.stop();
        }
        self.state = ConnState::Auth;
        info!("authenticated [{}]", self.addr);
    }

}

impl Handler<Sub> for Connection
{
    type Result = ();
    fn handle(&mut self, _msg: Sub, ctx: &mut Self::Context) {
        if let Some(ref mut cell) = self.cell {
            cell.write(sub(&self.topic, &self.channel));
        } else {
            error!("unable to subscribing: connection dropped [{}]", self.addr);
            ctx.stop();
        }
        self.state = ConnState::Ready;
        info!("subscribed [{}] topic: {} channel: {}", self.addr, self.topic, self.channel);
    }
}

impl Handler<NsqBackoff> for Connection
{
    type Result=();
    fn handle(&mut self, _msg: NsqBackoff, ctx: &mut Self::Context) {
        if let Some(timeout) = self.backoff.next_backoff() {
            if let Some(ref mut cell) = self.cell {
                cell.write(rdy(0));
                ctx.run_later(timeout, |_, ctx| ctx.notify(Resume));
                self.state = ConnState::Backoff;
            } else {
                error!("backoff failed: connection dropped [{}]", self.addr);
                Self::add_stream(once::<Cmd, Error>(Err(Error::NotConnected)), ctx);
            }
        }
    }
}

impl Handler<Resume> for Connection
{
    type Result=();
    fn handle(&mut self, _msg: Resume, ctx: &mut Self::Context) {
        if let Some(ref mut cell) = self.cell {
            cell.write(rdy(1));
            self.state = ConnState::Resume;
        } else {
            error!("resume failed: connection dropped [{}]", self.addr);
            Self::add_stream(once::<Cmd, Error>(Err(Error::NotConnected)), ctx);
        }
    }
}

impl<M: NsqMsg> Handler<AddHandler<M>> for Connection
{
    type Result=();
    fn handle(&mut self, msg: AddHandler<M>, _: &mut Self::Context) {
        let msg_id = TypeId::of::<Recipient<M>>();
        if msg_id == TypeId::of::<Recipient<Msg>>() {
            self.handlers.push(Box::new(msg.0));
            info!("Reader added");
        } else if msg_id == TypeId::of::<Recipient<InFlight>>() {
            self.info_hashmap.insert(msg_id, Box::new(msg.0));
            info!("inflight handler added");
        }
    }
}

impl Supervised for Connection
{
    fn restarting(&mut self, ctx: &mut Self::Context) {
        if self.state == ConnState::Stopped {
            ctx.stop();
        }
    }
}