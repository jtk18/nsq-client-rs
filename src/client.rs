use std::process;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam::channel::{self, Receiver, Sender};
use log::{debug, error, info};

use mio::{Events, Poll, PollOpt, Ready, Registration, Token};
use serde_json;

use crate::codec::decode_msg;
use crate::conn::{Conn, State, CONNECTION};
use crate::config::{Config, NsqdConfig};
use crate::msgs::{Cmd, Msg, Nop, NsqCmd, ConnMsg, ConnMsgInfo, ConnInfo};
use crate::reader::Consumer;

use bytes::BytesMut;

const CLIENT_TOKEN: Token = Token(1);
const CMD_TOKEN: Token = Token(2);

#[derive(Clone, Debug)]
pub(crate) struct CmdChannel(pub Sender<Cmd>, pub Receiver<Cmd>);

impl CmdChannel {
    pub fn new() -> CmdChannel {
        let (cmd_s, cmd_r) = channel::unbounded();
        CmdChannel(cmd_s, cmd_r)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MsgChannel(pub Sender<BytesMut>, pub Receiver<BytesMut>);

impl MsgChannel {
    pub fn new() -> MsgChannel {
        let (msg_s, msg_r) = channel::unbounded();
        MsgChannel(msg_s, msg_r)
    }
}

pub(crate) struct Sentinel(pub Sender<()>, Receiver<()>);

impl Sentinel {
    fn new() -> Sentinel {
        let (s, r) = channel::unbounded();
        Sentinel(s, r)
    }
}

pub struct Client<C, S>
where
    C: Into<String> + Clone,
    S: Into<String> + Clone,
{
    rdy: u32,
    max_attemps: u16,
    channel: String,
    topic: String,
    addr: String,
    config: Config<C>,
    secret: Option<S>,
    msg_channel: MsgChannel,
    cmd_channel: CmdChannel,
    sentinel: Sentinel,
    in_cmd: Receiver<ConnMsg>,
    out_info: Sender<ConnMsgInfo>,
    connected_s: Sender<bool>,
    connected_r: Receiver<bool>,
}

impl<C, S> Client<C, S>
where
    C: Into<String> + Clone,
    S: Into<String> + Clone,
{
    pub fn new(
        topic: S,
        channel: S,
        addr: S,
        config: Config<C>,
        secret: Option<S>,
        rdy: u32,
        max_attemps: u16,
        in_cmd: Receiver<ConnMsg>,
        out_info: Sender<ConnMsgInfo>,
    ) -> Client<C, S> {
        let (s, r): (Sender<bool>, Receiver<bool>) = channel::unbounded();
        Client {
            topic: topic.into(),
            channel: channel.into(),
            addr: addr.into(),
            config,
            rdy,
            secret,
            max_attemps,
            msg_channel: MsgChannel::new(),
            cmd_channel: CmdChannel::new(),
            sentinel: Sentinel::new(),
            in_cmd,
            out_info,
            connected_s: s,
            connected_r: r,
        }
    }

    pub fn run(&mut self) {
        let (handler, set_readiness) = Registration::new2();
        let r_sentinel = self.sentinel.1.clone();
        thread::spawn(move || loop {
            if let Ok(_ok) = r_sentinel.recv() {
                if let Err(e) = set_readiness.set_readiness(Ready::writable()) {
                    error!("error on handles waker: {}", e);
                }
            }
        });
        let (cmd_handler, cmd_readiness) = Registration::new2();
        let r_cmd = self.in_cmd.clone();
        let (s_inner_cmd, r_inner_cmd): (Sender<ConnMsg>, Receiver<ConnMsg>) = channel::unbounded();
        thread::spawn(move || loop {
            if let Ok(msg) = r_cmd.recv() {
                if let Err(e) = cmd_readiness.set_readiness(Ready::readable()) {
                    error!("error on in cmd waker: {}", e);
                }
                let _ = s_inner_cmd.send(msg);
            } 
        });

        println!("Creating conn");
        let mut conn = Conn::new(
            self.addr.clone(),
            self.config.clone(),
            self.cmd_channel.1.clone(),
            self.msg_channel.0.clone(),
            self.out_info.clone(),
        );
        println!("Conn created");
        let mut poll = Poll::new().unwrap();
        let mut evts = Events::with_capacity(1024);
        conn.register(&mut poll);
        if let Err(e) = poll.register(&handler, CLIENT_TOKEN, Ready::writable(), PollOpt::edge()) {
            error!("registering handler");
            panic!("{}", e);
        }
        if let Err(e) = poll.register(&handler, CMD_TOKEN, Ready::readable(), PollOpt::edge()) {
            error!("registering handler");
            panic!("{}", e);
        }
        conn.magic();
        let mut nsqd_config: NsqdConfig = NsqdConfig::default();
        let mut last_heartbeat = Instant::now();
        loop {
            if let Err(e) = poll.poll(&mut evts, Some(Duration::new(45, 0))) {
                error!("polling events failed");
                panic!("{}", e);
            }
            // if last_heartbeat is not seen shutdown occurred.
            if last_heartbeat.elapsed() > Duration::new(45, 0) {
                // send fake message as closed connection event.
                let _ = self.msg_channel.0.send(BytesMut::new());
            }
            for ev in &evts {
                debug!("event: {:?}", ev);
                if ev.token() == CMD_TOKEN {
                    if let Ok(msg) = r_inner_cmd.try_recv() {
                        match msg {
                            ConnMsg::Close => {
                                let _ = conn.close();
                                let _ = self.msg_channel.0.send(BytesMut::new());
                            },
//                            ConnMsg::Connect => {
//                                let _ = conn.socket = connect()
//                            }
                            _ => {},
                        }
                    }
                    continue;
                }
                if ev.token() == CONNECTION {
                    if ev.readiness().is_readable() {
                        match conn.read() {
                            Ok(0) => {
                                if conn.need_response {
                                    conn.reregister(&mut poll, Ready::readable());
                                }
                                break;
                            }
                            Err(e) => {
                                if e.kind() != std::io::ErrorKind::WouldBlock {
                                    panic!("Error on reading socket: {:?}", e);
                                }
                                if let Err(e) = self.out_info.send(ConnMsgInfo::IsConnected(ConnInfo{ connected: false, last_time: 0 })) {
                                    panic!("{}", e);
                                }
                                break;
                            }
                            _ => {}
                        };
                        if conn.state != State::Started {
                            match conn.state {
                                State::Identify => {
                                    let resp = conn
                                        .get_response(format!(
                                            "[{}] failed to indentify",
                                            self.addr
                                        ))
                                        .unwrap();
                                    nsqd_config = serde_json::from_str(&resp)
                                        .expect("failed to decode identify response");
                                    info!("[{}] configuration: {:#?}", self.addr, nsqd_config);
                                    if nsqd_config.tls_v1 {
                                        conn.tls_enabled();
                                        conn.reregister(&mut poll, Ready::readable());
                                        break;
                                    };
                                    if nsqd_config.auth_required {
                                        if self.secret.is_none() {
                                            error!("[{}] authentication required", self.addr);
                                            error!("secret token needed");
                                            process::exit(1)
                                        }
                                        conn.state = State::Auth;
                                    } else {
                                        conn.state = State::Subscribe;
                                    }
                                }
                                State::Tls => {
                                    let resp = conn
                                        .get_response(format!(
                                            "[{}] tls handshake failed",
                                            self.addr
                                        ))
                                        .unwrap();
                                    info!("[{}] tls connection: {}", self.addr, resp);
                                    if nsqd_config.auth_required {
                                        if self.secret.is_none() {
                                            error!("[{}] authentication required", self.addr);
                                            error!("secret token needed");
                                            process::exit(1)
                                        }
                                        conn.state = State::Auth;
                                    } else {
                                        conn.state = State::Subscribe;
                                    }
                                }
                                State::Auth => {
                                    let resp = conn
                                        .get_response(format!(
                                            "[{}] authentication failed",
                                            self.addr
                                        ))
                                        .unwrap();
                                    info!("[{}] authentication {}", self.addr, resp);
                                    conn.state = State::Subscribe;
                                }
                                State::Subscribe => {
                                    let resp = conn
                                        .get_response(format!(
                                            "[{}] authentication failed",
                                            self.addr
                                        ))
                                        .unwrap();
                                    info!(
                                        "[{}] subscribe channel: {} topic: {} {}",
                                        self.addr, self.channel, self.topic, resp
                                    );
                                    conn.state = State::Rdy;
                                }
                                _ => {}
                            }
                            conn.need_response = false;
                        }
                        conn.reregister(&mut poll, Ready::writable());
                    } else if conn.state != State::Started {
                        match conn.state {
                            State::Identify => {
                                conn.identify();
                            }
                            State::Auth => match &self.secret {
                                Some(s) => {
                                    let secret = s.clone();
                                    conn.auth(secret.into());
                                }
                                None => {}
                            },
                            State::Subscribe => {
                                conn.subscribe(self.topic.clone(), self.channel.clone());
                            }
                            State::Rdy => {
                                conn.rdy(self.rdy);
                            }
                            _ => {}
                        }
                        if let Err(e) = conn.write() {
                            error!("writing on socket: {:?}", e);
                        };
                        if conn.need_response {
                            conn.reregister(&mut poll, Ready::readable());
                        } else {
                            conn.reregister(&mut poll, Ready::writable());
                        };
                    } else {
                        if conn.heartbeat {
                            last_heartbeat = Instant::now();
                            conn.write_cmd(Nop);
                            if let Err(e) = conn.write() {
                                error!("writing on socket: {:?}", e);
                            }
                            conn.heartbeat_done();
                        }
                        conn.write_messages();
                        conn.reregister(&mut poll, Ready::readable());
                    }
                } else {
                    conn.write_messages();
                }
            }
        }
    }

    #[cfg(not(feature = "async"))]
    pub fn spawn<H: Consumer>(&mut self, n_threads: usize, reader: H) {
        for _i in 0..n_threads {
            let mut boxed = Box::new(reader.clone());
            let cmd = self.cmd_channel.0.clone();
            let msg_ch = self.msg_channel.1.clone();
            let sentinel = self.sentinel.0.clone();
            let max_attemps = self.max_attemps;
            let conn_s = self.connected_r.clone();
            thread::spawn(move || {
                let mut ctx = Context::new(cmd, sentinel);
                info!("Handler spawned");
                loop {
                    if let Ok(ref mut msg) = msg_ch.recv() {
                        if msg.len() == 0 {
                            boxed.on_close(&mut ctx);
                            continue;
                        }
                        let msg = decode_msg(msg);
                        boxed.on_msg(Msg {
                            timestamp: msg.0,
                            attemps: msg.1,
                            id: msg.2,
                            body: msg.3,
                        }, &mut ctx);
                    }
                }
            });
        }
    }
}

//pub enum EventMsg {
//    Conn(Event),
//    Client(ConnMsg),
//}

#[derive(Debug)]
pub struct Context {
    cmd_s: Sender<Cmd>,
    sentinel: Sender<()>,
}

impl Context {
    fn new(cmd_s: Sender<Cmd>, sentinel: Sender<()>) -> Context {
        Context {
            cmd_s,
            sentinel: sentinel,
        }
    }

    pub fn send<C: NsqCmd>(&mut self, cmd: C) {
        let cmd = cmd.as_cmd();
        let _ = self.cmd_s.send(cmd);
        let _ = self.sentinel.send(());
    }
}
