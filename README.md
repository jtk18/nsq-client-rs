# NSQ Rust client [![Build Status](https://travis-ci.com/alex179ohm/nsq-client-rs.svg?branch=master)](https://travis-ci.com/alex179ohm/nsq-client-rs) [![Build status](https://ci.appveyor.com/api/projects/status/ov5ryj2r4iy2v7rp/branch/master?svg=true)](https://ci.appveyor.com/project/alex179ohm/nsq-client-rs/branch/master)
Sponsored by <a href="https://tngrm.io"><img src="https://tngrm.io/static/img/tngrm_black.svg" width="100"></a>
---
A [Actix](https://actix.rs/) based client implementation for the [NSQ](https://nsq.io) realtime message processing system.
Nsq-client it's designed to support by default multiple Readers for Multiple Connections, readers are routed per single connection by a round robin algorithm.

## Examples
- [Simple Processing Message](https://github.com/alex179ohm/nsq-client-rs/tree/master/examples/reader)
- [Simple Producer](https://github.com/alex179ohm/nsq-client-rs/tree/master/examples/producer)

### Simple Reader (SUB)
```rust
extern crate nsqueue;
extern crate actix;

use std::sync::Arc;

use actix::prelude::*;

use nsqueue::{Connection, Msg, Fin, Subscribe, Config};

struct MyReader {
    pub conn: Arc<Addr<Connection>>,
}

impl Actor for MyReader {
    type Context = Context<Self>;
    fn started(&mut self, ctx: &mut Self::Context) {
        self.subscribe::<Msg>(ctx, self.conn.clone());
    }
}

impl Handler<Msg> for MyReader {
    fn handle(&mut self, msg: Msg, _: &mut Self::Context) {
        println!("MyReader received {:?}", msg);
        self.conn.do_send(Fin(msg.id));
    }
}

fn main() {
    let sys = System::new("consumer");
    let config = Config::default().client_id("consumer");
    let c = Supervisor::start(|_| Connection::new(
        "test", // <- topic
        "test", // <- channel
        "0.0.0.0:4150", // <- nsqd tcp address
        Some(config), // <- config (Optional)
        None, // secret for Auth (Optional)
        Some(2) // <- RDY (Optional default: 1)
    ));
    let conn = Arc::new(c);
    let _ = MyReader{ conn: conn.clone() }.start(); // <- Same thread reader
    let _ = Arbiter::start(|_| MyReader{ conn: conn }); // <- start another reader in different thread
    sys.run();
}
```
### launch nsqd
```bash
$ nsqd -verbose
```
### launch the reader
```bash
$ RUST_LOG=nsq_client=debug cargo run
```
### launch the producer
```bash
$ cargo run
```

[![asciicast](https://asciinema.org/a/8dZ5QgjN3WCwDhgU8mAX9BMsR.svg)](https://asciinema.org/a/8dZ5QgjN3WCwDhgU8mAX9BMsR)

### Current features and work in progress
- [X] PUB
- [X] SUB
- [ ] Discovery
- [X] Backoff
- [ ] TLS
- [ ] Snappy
- [X] Auth
- [ ] First-ready-first-served readers routing algorithm.

## License

Licensed under
* MIT license (see [LICENSE](LICENSE) or <http://opensource.org/licenses/MIT>)
