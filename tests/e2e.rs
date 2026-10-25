mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};

struct Client(Child);
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
use std::net::TcpStream;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread;
use std::time::{Duration, Instant};
use support::sshd::{Sshd, descendants, enabled, group_exists, process_exists};

fn exercise_cleanup(panic_in_body: bool) {
    if !enabled() {
        eprintln!("skipping: set SSHTM_E2E_SSHD=1");
        return;
    }
    let fixture = Sshd::start().expect("start unprivileged sshd fixture");
    let effective = Command::new("/usr/bin/sshd")
        .args(["-T", "-f"])
        .arg(fixture.root().join("sshd_config"))
        .output()
        .unwrap();
    assert!(
        effective.status.success(),
        "{}",
        String::from_utf8_lossy(&effective.stderr)
    );
    assert!(
        String::from_utf8_lossy(&effective.stdout)
            .lines()
            .any(|line| line.eq_ignore_ascii_case("permituserrc no"))
    );
    let group = fixture.process_group();
    let root = fixture.root();
    let output = fixture.ssh().args(["fixture", "echo ok"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"ok\n");
    let mut echo = TcpStream::connect(fixture.echo_addr()).unwrap();
    echo.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    echo.write_all(b"echo").unwrap();
    let mut reply = [0; 4];
    echo.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"echo");
    drop(echo);

    // Killing the first sleep must not let this shell launch its next job.
    // Keep the marker inode open for inspection after tempdir removal.
    let marker_path = fixture.root().join("shell-advanced");
    let mut marker = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&marker_path)
        .unwrap();
    let quoted_path = format!(
        "'{}'",
        marker_path.display().to_string().replace('\'', "'\\''")
    );
    let script = format!("echo ready; sleep 60; echo advanced >> {quoted_path}; sleep 60 & wait");
    let command = format!("/bin/sh -c '{}'", script.replace('\'', "'\\''"));
    let mut client = Client(
        fixture
            .ssh()
            .args(["fixture", &command])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    BufReader::new(client.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "ready\n");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !descendants(group)
        .iter()
        .any(|pid| std::fs::read(format!("/proc/{pid}/comm")).is_ok_and(|name| name == b"sleep\n"))
        && Instant::now() < deadline
    {
        assert!(
            client.0.try_wait().unwrap().is_none(),
            "SSH session exited early"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let sessions = descendants(group);
    assert!(
        sessions.iter().any(|pid| {
            std::fs::read(format!("/proc/{pid}/comm")).is_ok_and(|name| name == b"sleep\n")
        }),
        "remote shell did not start its first sleep"
    );
    // The fixture is dropped while a session is still connected in both paths.
    let body = catch_unwind(AssertUnwindSafe(|| {
        let _fixture = fixture;
        if panic_in_body {
            panic!("intentional fixture-body panic");
        }
    }));
    let _ = client.0.kill();
    client.0.wait().unwrap();
    let mut advanced = String::new();
    marker.read_to_string(&mut advanced).unwrap();
    assert!(
        advanced.is_empty(),
        "remote shell ran its next command during teardown"
    );
    match body {
        Ok(()) => assert!(!panic_in_body, "expected deliberate fixture-body panic"),
        Err(payload) => {
            assert!(panic_in_body, "unexpected fixture-body panic");
            assert_eq!(
                payload.downcast_ref::<&str>(),
                Some(&"intentional fixture-body panic")
            );
        }
    }
    assert!(
        !root.exists(),
        "fixture tempdir survived: {}",
        root.display()
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while (group_exists(group) || sessions.iter().any(|pid| process_exists(*pid)))
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!group_exists(group), "sshd process group survived teardown");
    assert!(!process_exists(group), "sshd listener survived teardown");
    for pid in sessions {
        assert!(!process_exists(pid), "sshd session {pid} survived teardown");
    }
}

#[test]
#[ignore = "requires SSHTM_E2E_SSHD=1 and unprivileged OpenSSH"]
fn e2e_sshd_cleanup_success() {
    exercise_cleanup(false);
}

#[test]
#[ignore = "requires SSHTM_E2E_SSHD=1 and unprivileged OpenSSH"]
fn e2e_sshd_cleanup_panic() {
    exercise_cleanup(true);
}
