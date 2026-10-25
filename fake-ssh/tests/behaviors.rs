use std::{
    io::Read,
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};

struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const FORCED: [&str; 5] = [
    "BatchMode=yes",
    "ExitOnForwardFailure=yes",
    "ControlMaster=no",
    "ControlPath=none",
    "ForkAfterAuthentication=no",
];

fn command(host: &str, port: u16) -> Command {
    command_with_options(host, port, &FORCED)
}

fn command_with_options(host: &str, port: u16, options: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ssh"));
    cmd.args(["-N", "-T"]);
    for option in options {
        cmd.args(["-o", option]);
    }
    cmd.args(["-L", &format!("127.0.0.1:{port}:localhost:1"), "--", host]);
    cmd.stderr(Stdio::piped());
    cmd
}

fn port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn started(host: &str) -> (Guard, u16) {
    let port = port();
    let child = Guard(command(host, port).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return (child, port);
        }
        assert!(Instant::now() < deadline, "fake-ssh did not listen");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn ok_accepts_connections() {
    let (_child, port) = started("ok");
    TcpStream::connect(("127.0.0.1", port)).unwrap();
}

#[test]
fn fail_is_immediate() {
    let start = Instant::now();
    let output = command("fail", port()).output().unwrap();
    assert!(start.elapsed() < Duration::from_millis(100));
    assert_eq!(output.status.code(), Some(255));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Permission denied (publickey)."));
}

#[test]
fn config_probe_supports_keepalive() {
    for (host, interval) in [("ok", 0), ("keepalive", 30)] {
        let output = Command::new(env!("CARGO_BIN_EXE_ssh"))
            .args(["-G", "--", host])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("serveraliveinterval {interval}\nserveralivecountmax 3\n")
        );
    }
}

#[test]
fn missing_forced_options_are_rejected() {
    for missing in FORCED {
        let options: Vec<_> = FORCED
            .into_iter()
            .filter(|option| *option != missing)
            .collect();
        let output = command_with_options("fail", port(), &options)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(255));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("missing forced option {missing}"))
        );
    }
}

#[test]
fn conflicting_forced_options_are_rejected_in_both_orders() {
    for conflict in [
        "batchmode=no",
        "exitonforwardfailure=no",
        "controlmaster=auto",
        "controlpath=/tmp/master",
        "forkafterauthentication=yes",
    ] {
        for first in [true, false] {
            let mut options = FORCED.to_vec();
            options.insert(if first { 0 } else { options.len() }, conflict);
            let output = command_with_options("fail", port(), &options)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(255));
            assert!(String::from_utf8_lossy(&output.stderr).contains("conflicting forced option"));
        }
    }
}

#[test]
fn alternate_option_syntax_is_rejected_in_both_orders() {
    for option in [
        "BatchMode no",
        "BatchMode = no",
        " BatchMode=no",
        "ControlMaster auto",
        "BatchMode\tno",
    ] {
        for first in [true, false] {
            let mut options = FORCED.to_vec();
            options.insert(if first { 0 } else { options.len() }, option);
            let output = command_with_options("fail", port(), &options)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(255));
            assert!(String::from_utf8_lossy(&output.stderr).contains("noncanonical -o option"));
        }
    }
}

#[test]
fn hang_does_not_listen() {
    let port = port();
    let mut child = Guard(command("hang", port).spawn().unwrap());
    thread::sleep(Duration::from_millis(100));
    assert!(child.0.try_wait().unwrap().is_none());
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
}

#[test]
fn flap_exits_after_one_second() {
    let start = Instant::now();
    let (mut child, _) = started("flap");
    let deadline = start + Duration::from_secs(3);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert_eq!(status.code(), Some(255));
            assert!(start.elapsed() >= Duration::from_secs(1));
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn stubborn_ignores_sigterm() {
    let (mut child, port) = started("stubborn");
    kill(Pid::from_raw(child.0.id() as i32), Signal::SIGTERM).unwrap();
    thread::sleep(Duration::from_millis(100));
    assert!(child.0.try_wait().unwrap().is_none());
    TcpStream::connect(("127.0.0.1", port)).unwrap();
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let mut stderr = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.contains("fake-ssh: stubborn"));
}
