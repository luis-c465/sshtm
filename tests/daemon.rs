//! R2 tests use the library wire client and hidden daemon, not the R3 CLI.
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn fake_dir() -> PathBuf {
    std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned()
}
fn request(socket: &Path, command: Value) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut command = command;
    command["v"] = json!(1);
    writeln!(stream, "{command}").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}
fn data(response: Value) -> Value {
    assert_eq!(response["status"], "ok", "{response}");
    response["data"].clone()
}
fn open(host: &str, port: Option<u16>) -> Value {
    json!({"command":"open","host":host,"forward":{"local":port,"rhost":"localhost","rport":1},"auth_sock":null})
}
fn port(row: &Value) -> u16 {
    row["local"]
        .as_str()
        .unwrap()
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}
fn wait(mut predicate: impl FnMut() -> bool, within: Duration) {
    let end = Instant::now() + within;
    while !predicate() {
        assert!(Instant::now() < end, "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}
struct Daemon {
    root: tempfile::TempDir,
    socket: PathBuf,
    child: Child,
}
impl Daemon {
    fn new(idle: &str) -> Self {
        Self::with_path(idle, &fake_dir(), false)
    }
    fn with_path(idle: &str, path: &Path, stale: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("sshtm");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("sock");
        if stale {
            std::fs::write(&socket, "stale").unwrap();
        }
        let child = Command::new(env!("CARGO_BIN_EXE_sshtm"))
            .args(["daemon", "--idle-exit", idle])
            .env("XDG_RUNTIME_DIR", root.path())
            .env("PATH", path)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut daemon = Self {
            root,
            socket,
            child,
        };
        wait(
            || {
                assert!(
                    daemon.child.try_wait().unwrap().is_none(),
                    "daemon exited before ready"
                );
                UnixStream::connect(&daemon.socket).is_ok()
            },
            Duration::from_secs(3),
        );
        daemon
    }
    fn call(&self, value: Value) -> Value {
        request(&self.socket, value)
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            let _ = kill(Pid::from_raw(self.child.id() as i32), Signal::SIGTERM);
            let end = Instant::now() + Duration::from_secs(4);
            while self.child.try_wait().unwrap().is_none() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
#[test]
fn indices_concurrency_duplicate_and_logs() {
    let daemon = Daemon::new("30");
    let rows: Vec<_> = std::thread::scope(|scope| {
        let tasks: Vec<_> = (0..10)
            .map(|_| scope.spawn(|| data(daemon.call(open("ok", None)))))
            .collect();
        tasks.into_iter().map(|t| t.join().unwrap()).collect()
    });
    let mut indices: Vec<_> = rows.iter().map(|r| r["idx"].as_u64().unwrap()).collect();
    indices.sort();
    assert_eq!(indices, (1..=10).collect::<Vec<_>>());
    let port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let responses = std::thread::scope(|scope| {
        let a = scope.spawn(|| daemon.call(open("ok", Some(port))));
        let b = scope.spawn(|| daemon.call(open("ok", Some(port))));
        [a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(responses.iter().filter(|r| r["status"] == "ok").count(), 1);
    let winner = responses.iter().find(|r| r["status"] == "ok").unwrap()["data"]["idx"]
        .as_u64()
        .unwrap();
    assert!(
        responses.iter().find(|r| r["status"] == "error").unwrap()["message"]
            .as_str()
            .unwrap()
            .contains(&format!("tunnel {winner}"))
    );
    data(daemon.call(json!({"command":"kill","indices":[2],"all":false})));
    let two = data(daemon.call(open("ok", None)));
    assert_eq!(two["idx"], 2);
    let logs = data(daemon.call(json!({"command":"logs","index":2})));
    assert!(logs.as_array().unwrap().iter().any(|s| s == "fake-ssh: ok"));
    let partial = daemon.call(json!({"command":"kill","indices":[1,999],"all":false}));
    assert!(
        partial["message"]
            .as_str()
            .unwrap()
            .contains("no tunnel 999")
    );
    assert!(
        !data(daemon.call(json!({"command":"list"})))
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["idx"] == 1)
    );
    data(daemon.call(json!({"command":"kill","indices":[],"all":true})));
    for row in rows {
        let local: u16 = row["local"]
            .as_str()
            .unwrap()
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(TcpListener::bind(("127.0.0.1", local)).is_ok());
    }
}
#[test]
fn stable_index_during_reconnect_and_starting_cancellation() {
    let daemon = Daemon::new("30");
    data(daemon.call(open("ok", None)));
    data(daemon.call(open("ok", None)));
    let third = data(daemon.call(open("flap", None)));
    assert_eq!(third["idx"], 3);
    data(daemon.call(json!({"command":"kill","indices":[1,2],"all":false})));
    wait(
        || data(daemon.call(json!({"command":"list"})))[0]["state"] == "reconnecting",
        Duration::from_secs(3),
    );
    wait(
        || data(daemon.call(json!({"command":"list"})))[0]["state"] == "up",
        Duration::from_secs(3),
    );
    assert_eq!(data(daemon.call(json!({"command":"list"})))[0]["idx"], 3);
    std::thread::scope(|scope| {
        let opening = scope.spawn(|| daemon.call(open("hang", None)));
        wait(
            || {
                data(daemon.call(json!({"command":"list"})))
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["state"] == "starting")
            },
            Duration::from_secs(2),
        );
        data(daemon.call(json!({"command":"kill","indices":[1],"all":false})));
        assert_eq!(opening.join().unwrap()["status"], "error");
    });
}
#[test]
fn stale_socket_permissions_protocol_and_idle_exit() {
    let mut daemon = Daemon::with_path("1", &fake_dir(), true);
    let dir = daemon.root.path().join("sshtm");
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&daemon.socket)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let mut stream = UnixStream::connect(&daemon.socket).unwrap();
    writeln!(stream, "{}", json!({"v":99,"command":"ping"})).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    assert!(line.contains("protocol 1"));
    let mut stream = UnixStream::connect(&daemon.socket).unwrap();
    // Stop writing when the daemon rejects the bounded prefix and closes.
    let _ = stream.write_all(&vec![b'x'; 1024 * 1024]);
    let mut line = String::new();
    let _ = BufReader::new(stream).read_line(&mut line);
    assert!(line.contains("error"));
    let row = data(daemon.call(open("ok", None)));
    data(daemon.call(json!({"command":"kill","indices":[row["idx"]],"all":false})));
    wait(
        || daemon.child.try_wait().unwrap().is_some(),
        Duration::from_secs(3),
    );
    assert!(!daemon.socket.exists());
}
fn running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .is_some_and(|s| s.rsplit_once(") ").unwrap().1.as_bytes()[0] != b'Z')
}
#[test]
fn signal_cleanup_and_parent_death() {
    for signal in [Signal::SIGTERM, Signal::SIGKILL] {
        let mut daemon = Daemon::new("30");
        let mut rows = Vec::new();
        for _ in 0..3 {
            rows.push(data(daemon.call(open(
                if signal == Signal::SIGTERM {
                    "stubborn"
                } else {
                    "ok"
                },
                None,
            ))));
        }
        kill(Pid::from_raw(daemon.child.id() as i32), signal).unwrap();
        let limit = if signal == Signal::SIGTERM {
            Duration::from_secs(3)
        } else {
            Duration::from_secs(1)
        };
        wait(
            || {
                rows.iter()
                    .all(|r| !running(r["pid"].as_u64().unwrap() as u32))
            },
            limit,
        );
        for row in rows {
            assert!(TcpListener::bind(("127.0.0.1", port(&row))).is_ok());
        }
        daemon.child.wait().unwrap();
        if signal == Signal::SIGTERM {
            assert!(!daemon.socket.exists());
        }
    }
}
#[test]
#[ignore = "explicit memory measurement, creates 100 forwards with saturated rings"]
fn daemon_memory_100() {
    let wrapper = tempfile::tempdir().unwrap();
    let script = wrapper.path().join("ssh");
    std::fs::write(&script,format!("#!/usr/bin/python3\nimport os,sys\nif sys.argv[1] != '-G':\n for _ in range(200): os.write(2,b'\\xff'*4095+b'\\n')\nos.execv({:?},[{:?}]+sys.argv[1:])\n",fake_dir().join("ssh"),fake_dir().join("ssh"))).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let daemon = Daemon::with_path("30", wrapper.path(), false);
    fn rss(pid: u32) -> String {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .unwrap()
            .lines()
            .find(|l| l.starts_with("VmRSS:"))
            .unwrap()
            .to_string()
    }
    println!("baseline {}", rss(daemon.child.id()));
    for _ in 0..100 {
        data(daemon.call(open("ok", None)));
    }
    wait(
        || {
            data(daemon.call(json!({"command":"logs","index":100})))
                .as_array()
                .unwrap()
                .len()
                == 200
        },
        Duration::from_secs(3),
    );
    std::thread::sleep(Duration::from_millis(200));
    println!(
        "100 tunnels, full 4095-byte invalid UTF-8 input rings: {}",
        rss(daemon.child.id())
    );
    data(daemon.call(json!({"command":"shutdown"})));
}
