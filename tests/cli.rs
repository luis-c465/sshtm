//! Public CLI contracts, using isolated autostarted daemons and fake SSH.
use serde_json::Value;
use std::{
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Command, Output},
    time::{Duration, Instant},
};

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}
fn running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(") ").map(|(_, rest)| !rest.starts_with('Z')))
        .unwrap_or(false)
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
fn wait(mut check: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < end, "timed out waiting for CLI state");
        std::thread::sleep(Duration::from_millis(20));
    }
}
struct Cli {
    root: tempfile::TempDir,
    children: Vec<(u32, u16)>,
}
impl Cli {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            children: Vec::new(),
        }
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sshtm"));
        let fake_dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        cmd.env("XDG_RUNTIME_DIR", self.root.path())
            .env("PATH", fake_dir)
            .env_remove("SSH_AUTH_SOCK");
        cmd
    }
    fn rows(&self) -> Vec<Value> {
        let output = self.command().args(["ls", "--json"]).output().unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn record(&mut self) {
        for row in self.rows() {
            if let Some(pid) = row["pid"].as_u64() {
                let child = (pid as u32, port(&row));
                if !self.children.contains(&child) {
                    self.children.push(child);
                }
            }
        }
    }
    fn run(&mut self, args: &[&str], code: i32) -> Output {
        let output = self.command().args(args).output().unwrap();
        // Record children even when the following status assertion fails.
        if self.root.path().join("sshtm/sock").exists() {
            self.record();
        }
        assert_eq!(
            output.status.code(),
            Some(code),
            "{args:?}: stdout={} stderr={}",
            text(&output.stdout),
            text(&output.stderr)
        );
        output
    }
    fn assert_no_daemon(&self) {
        assert!(!self.root.path().join("sshtm/sock").exists());
        assert!(!self.root.path().join("sshtm/daemon.log").exists());
    }
}
impl Drop for Cli {
    fn drop(&mut self) {
        let shutdown = self.command().arg("shutdown").output();
        let end = Instant::now() + Duration::from_secs(5);
        let clean = loop {
            let clean = self.children.iter().all(|&(pid, port)| {
                !running(pid) && TcpListener::bind(("127.0.0.1", port)).is_ok()
            }) && !self.root.path().join("sshtm/sock").exists();
            if clean || Instant::now() >= end {
                break clean;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if !std::thread::panicking() {
            assert!(shutdown.unwrap().status.success(), "shutdown failed");
            assert!(
                clean,
                "CLI teardown left a child, listener, or socket behind"
            );
        }
    }
}

#[test]
fn multiple_opens_table_json_and_auto_port_connect() {
    let mut cli = Cli::new();
    let out = cli.run(&["open", "ok", "0:5432", "0:db:6379"], 0);
    let rows = cli.rows();
    assert_eq!(rows.len(), 2);
    let expected = format!(
        "1 127.0.0.1:{} -> localhost:5432\n2 127.0.0.1:{} -> db:6379\n",
        port(&rows[0]),
        port(&rows[1])
    );
    assert_eq!(text(&out.stdout), expected);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row["idx"], i + 1);
        assert_eq!(row["host"], "ok");
        assert_eq!(row["state"], "up");
        let mut fields: Vec<_> = row.as_object().unwrap().keys().cloned().collect();
        fields.sort();
        let snapshot: Vec<String> =
            serde_json::from_str(include_str!("snapshots/ls-fields.json")).unwrap();
        assert_eq!(fields, snapshot);
        TcpStream::connect(("127.0.0.1", port(row))).unwrap();
    }
    for args in [vec![], vec!["ls"]] {
        let table = text(&cli.run(&args, 0).stdout);
        let header: Vec<_> = table.lines().next().unwrap().split_whitespace().collect();
        assert_eq!(
            header,
            ["IDX", "HOST", "LOCAL", "REMOTE", "STATE", "UPTIME"]
        );
        assert_eq!(table.lines().count(), 3);
        assert!(table.contains("localhost:5432") && table.contains("db:6379"));
        assert_eq!(
            table
                .lines()
                .filter(|s| s.split_whitespace().any(|w| w == "up"))
                .count(),
            2
        );
    }
}

#[test]
fn failure_hint_and_sequential_operation_failures() {
    let mut cli = Cli::new();
    let failed = cli.run(&["open", "fail", "0:5432", "0:6379"], 1);
    let error = text(&failed.stderr);
    assert_eq!(error.matches("Permission denied (publickey).").count(), 2);
    assert!(error.contains("run 'ssh fail' once interactively"));
    assert!(cli.rows().is_empty());
    cli.run(&["open", "ok", "0:5432"], 0);
    let local = port(&cli.rows()[0]);
    let duplicate = format!("{local}:5432");
    let out = cli.run(&["open", "ok", &duplicate, "0:6379"], 1);
    assert!(text(&out.stderr).contains("tunnel 1"));
    assert!(text(&out.stdout).starts_with("2 127.0.0.1:"));
    assert_eq!(cli.rows().len(), 2);
}

#[test]
fn partial_range_all_kill_and_index_reuse() {
    let mut cli = Cli::new();
    cli.run(&["open", "ok", "0:1", "0:2", "0:3", "0:4"], 0);
    let out = cli.run(&["kill", "1", "9"], 1);
    assert!(text(&out.stderr).contains("no tunnel 9"));
    assert_eq!(
        cli.rows()
            .iter()
            .map(|r| r["idx"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [2, 3, 4]
    );
    cli.run(&["kill", "2-4"], 0);
    assert!(cli.rows().is_empty());
    cli.run(&["open", "ok", "0:1", "0:2"], 0);
    assert_eq!(cli.rows()[0]["idx"], 1);
    cli.run(&["kill", "--all"], 0);
    assert!(cli.rows().is_empty());
    for &(pid, port) in &cli.children {
        assert!(!running(pid));
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    }
}

#[test]
fn read_only_commands_do_not_autostart() {
    let mut cli = Cli::new();
    for args in [vec![], vec!["ls"], vec!["logs", "1"]] {
        let out = cli.run(&args, 0);
        assert!(format!("{}{}", text(&out.stdout), text(&out.stderr)).contains("no tunnels"));
        cli.assert_no_daemon();
    }
    assert_eq!(text(&cli.run(&["ls", "--json"], 0).stdout).trim(), "[]");
    let missing = cli.run(&["kill", "1"], 1);
    assert!(text(&missing.stderr).contains("no tunnel 1"));
    cli.run(&["kill", "--all"], 0);
    let shutdown = cli.run(&["shutdown"], 0);
    assert!(shutdown.stdout.is_empty() && shutdown.stderr.is_empty());
    cli.assert_no_daemon();
}

#[test]
fn invalid_usage_is_validated_before_autostart() {
    let mut cli = Cli::new();
    for args in [
        vec!["open"],
        vec!["open", "ok"],
        vec!["open", "ok", "0:1", "70000"],
        vec!["open", "a b", "0:1"],
        vec!["open", "", "0:1"],
        vec!["open", "--", "-oProxyCommand=x", "0:1"],
        vec!["kill"],
        vec!["kill", "0"],
        vec!["kill", "4-2"],
        vec!["kill", "1-x"],
        vec!["kill", "--all", "1"],
        vec!["logs"],
        vec!["logs", "0"],
        vec!["ls", "--unknown"],
        vec!["unknown"],
    ] {
        let output = cli.run(&args, 2);
        let error = text(&output.stderr);
        assert!(
            error.contains("error:") && error.contains("--help"),
            "{args:?}: {error}"
        );
        cli.assert_no_daemon();
    }
}

#[test]
fn logs_preserve_stderr_order() {
    let mut cli = Cli::new();
    cli.run(&["open", "flap", "0:1"], 0);
    wait(|| {
        let out = cli.command().args(["logs", "1"]).output().unwrap();
        let logs = text(&out.stdout);
        logs.contains("fake-ssh: connection lost")
    });
    let out = cli.run(&["logs", "1"], 0);
    let logs = text(&out.stdout);
    assert!(logs.find("fake-ssh: flap").unwrap() < logs.find("fake-ssh: connection lost").unwrap());
    cli.run(&["logs", "999"], 1);
}

#[test]
fn each_open_carries_its_own_agent_socket() {
    let mut cli = Cli::new();
    for agent in [
        Some("/tmp/sshtm-test-agent-a"),
        Some("/tmp/sshtm-test-agent-b"),
        None,
    ] {
        let mut command = cli.command();
        command.args(["open", "ok", "0:1"]);
        if let Some(agent) = agent {
            command.env("SSH_AUTH_SOCK", agent);
        }
        let output = command.output().unwrap();
        cli.record();
        assert!(output.status.success(), "{}", text(&output.stderr));
        let rows = cli.rows();
        let pid = rows.last().unwrap()["pid"].as_u64().unwrap();
        let environment = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
        let actual = environment
            .split(|&b| b == 0)
            .find_map(|entry| entry.strip_prefix(b"SSH_AUTH_SOCK="));
        assert_eq!(actual, agent.map(str::as_bytes));
    }
}

#[test]
fn dynamic_index_completion_and_no_autostart() {
    let mut cli = Cli::new();
    let complete = |cli: &Cli, verb: &str, prefix: &str| {
        let output = cli
            .command()
            .env("COMPLETE", "fish")
            .args(["--", "sshtm", verb, prefix])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        text(&output.stdout)
    };
    assert!(
        complete(&cli, "kill", "")
            .lines()
            .all(|line| !line.starts_with("1\t"))
    );
    cli.assert_no_daemon();
    cli.run(&["open", "ok", "0:1", "0:2"], 0);
    for verb in ["kill", "logs"] {
        let output = complete(&cli, verb, "");
        assert!(output.lines().any(|line| line == "1\tok"), "{output}");
        assert!(output.lines().any(|line| line == "2\tok"), "{output}");
        assert_eq!(complete(&cli, verb, "2"), "2\tok\n");
        assert!(complete(&cli, verb, "999").is_empty());
    }
}

#[test]
fn completion_registration_shell_syntax() {
    use std::{io::Write, process::Stdio};
    let mut cli = Cli::new();
    for shell in ["bash", "zsh", "fish"] {
        let script = cli.run(&["completions", shell], 0).stdout;
        assert!(text(&script).contains("COMPLETE="));
        // Optional shell tools, never required by the default test suite.
        let child = Command::new(shell)
            .arg("-n")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("{shell}: {error}"),
        };
        child.stdin.take().unwrap().write_all(&script).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{shell}: {}", text(&output.stderr));
    }
    cli.run(&["completions", "powershell"], 2);
    cli.assert_no_daemon();
}

#[test]
fn panic_cleanup_releases_children_ports_and_socket() {
    let mut cli = Cli::new();
    cli.run(&["open", "ok", "0:1"], 0);
    let children = cli.children.clone();
    let socket: PathBuf = cli.root.path().join("sshtm/sock");
    let result = std::panic::catch_unwind(move || {
        let _guard = cli;
        panic!("deliberate CLI fixture panic");
    });
    assert_eq!(
        result.unwrap_err().downcast_ref::<&str>(),
        Some(&"deliberate CLI fixture panic")
    );
    assert!(!socket.exists());
    for (pid, port) in children {
        assert!(!running(pid));
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    }
}
