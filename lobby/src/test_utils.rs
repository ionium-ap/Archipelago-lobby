// For tests that need a Valkey. They need a `valkey-server` binary in the PATH, like the tests
// of the `wq` crate.

use std::{
    net::TcpListener,
    process::{Child, Command, Stdio},
    time::Duration,
};

use deadpool_redis::Pool as RedisPool;

pub(crate) struct ValkeyInstance {
    port: u16,
    process: Child,
}

impl Drop for ValkeyInstance {
    fn drop(&mut self) {
        let _ = self.process.kill();
    }
}

impl ValkeyInstance {
    pub(crate) fn url(&self) -> String {
        format!("redis://127.0.0.1:{}?protocol=resp3", self.port)
    }
}

pub(crate) fn start_valkey() -> ValkeyInstance {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let process = Command::new("valkey-server")
        .arg("--port")
        .arg(port.to_string())
        .stdout(Stdio::null())
        .spawn()
        .expect("Failed to start valkey-server, is it in the PATH?");
    let instance = ValkeyInstance { port, process };

    for _ in 0..100 {
        let client = redis::Client::open(instance.url()).unwrap();
        if client.get_connection().is_ok() {
            return instance;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    panic!("Failed to start valkey on port {}", instance.port);
}

pub(crate) fn redis_pool(valkey: &ValkeyInstance) -> RedisPool {
    deadpool_redis::Config::from_url(valkey.url())
        .create_pool(Some(deadpool_redis::Runtime::Tokio1))
        .unwrap()
}
