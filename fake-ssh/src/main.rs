//! Test-only OpenSSH stand-in. No network connections are made to remote hosts.
use std::{
    env,
    net::TcpListener,
    process, thread,
    time::{Duration, Instant},
};

use nix::sys::signal::{self, SigHandler, Signal};

fn run(args: &[String]) -> Result<(), String> {
    if args.len() == 3 && args[0] == "-G" && args[1] == "--" {
        println!(
            "serveraliveinterval {}",
            if args[2] == "keepalive" { 30 } else { 0 }
        );
        println!("serveralivecountmax 3");
        return Ok(());
    }

    let mut options = Vec::new();
    let mut forward = None;
    let mut no_command = false;
    let mut no_tty = false;
    let mut i = 0;
    while i < args.len() && args[i] != "--" {
        match args[i].as_str() {
            "-N" => no_command = true,
            "-T" => no_tty = true,
            "-o" | "-L" => {
                let value = args.get(i + 1).ok_or("missing option value")?;
                if args[i] == "-o" {
                    // The stand-in accepts only the argv builder's canonical form.
                    // Otherwise OpenSSH's alternate whitespace syntax could hide
                    // a conflicting value that takes precedence over a forced one.
                    if !value.contains('=') || value.chars().any(char::is_whitespace) {
                        return Err(format!("noncanonical -o option {value:?}"));
                    }
                    options.push(value.as_str());
                } else if forward.replace(value.as_str()).is_some() {
                    return Err("expected exactly one -L".into());
                }
                i += 1;
            }
            other => return Err(format!("unexpected argument {other}")),
        }
        i += 1;
    }
    if !no_command || !no_tty || i + 2 != args.len() || args[i] != "--" {
        return Err("expected -N -T and -- <host>".into());
    }
    for forced in [
        "BatchMode=yes",
        "ExitOnForwardFailure=yes",
        "ControlMaster=no",
        "ControlPath=none",
        "ForkAfterAuthentication=no",
    ] {
        let (name, expected) = forced.split_once('=').unwrap();
        let mut present = false;
        for option in &options {
            if let Some((key, value)) = option.split_once('=')
                && key.eq_ignore_ascii_case(name)
            {
                if value != expected {
                    return Err(format!(
                        "conflicting forced option {option}; expected {forced}"
                    ));
                }
                present = true;
            }
        }
        if !present {
            return Err(format!("missing forced option {forced}"));
        }
    }
    let host = &args[i + 1];
    let forward = forward.ok_or("missing -L")?;
    let rest = forward
        .strip_prefix("127.0.0.1:")
        .ok_or("bind address must be 127.0.0.1")?;
    let (port, remote) = rest.split_once(':').ok_or("missing remote address")?;
    let port: u16 = port.parse().map_err(|_| "invalid local port")?;
    if port == 0 || remote.is_empty() {
        return Err("expected allocated local port and remote address".into());
    }
    match host.as_str() {
        "fail" => return Err("Permission denied (publickey).".into()),
        "ok" | "keepalive" | "hang" | "flap" | "stubborn" => {}
        _ => return Err(format!("unknown fake host {host}")),
    }
    if host == "stubborn" {
        // SAFETY: SIG_IGN installs no Rust handler and accesses no shared state.
        unsafe { signal::signal(Signal::SIGTERM, SigHandler::SigIgn) }
            .map_err(|e| e.to_string())?;
    }
    let listener = if host == "hang" {
        None
    } else {
        Some(TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("bind: {e}"))?)
    };
    if let Some(listener) = &listener {
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    }
    eprintln!("fake-ssh: {host}");
    let started = Instant::now();
    loop {
        if host == "flap" && started.elapsed() >= Duration::from_secs(1) {
            return Err("fake-ssh: connection lost".into());
        }
        if let Some(listener) = &listener {
            match listener.accept() {
                Ok((stream, _)) => drop(stream),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn main() {
    if let Err(error) = run(&env::args().skip(1).collect::<Vec<_>>()) {
        eprintln!("{error}");
        process::exit(255);
    }
}
