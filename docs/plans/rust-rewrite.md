# sshtm (Rust rewrite) — Implementation plan

## Document status

This top-level plan orders the greenfield rewrite of `sshtm` in Rust. The new
tool is a small command-line front end that runs and supervises local port
forwards with the system OpenSSH client, and reads only the user's existing
`~/.ssh/config`. There is no product spec, architecture document, or roadmap
yet. The decisions below, approved by the user on 2026-10-24, serve as those
documents until R6 writes the README.

The rewrite carries nothing over from the Go implementation (`sshtmd`, gRPC
on `127.0.0.1:50051`, the machine and profile stores, the TUI, active-tunnel
restore, legacy migrations, packaging). It is not compatible with any of it.

The plan was drafted in the Go repository and moved here on 2026-10-24, when
this repository was initialized.

Upstream facts were checked against OpenSSH 10.6p1 (Arch/CachyOS build) on
Linux 7.2.9. Crate versions are those on crates.io on 2026-10-24. R0 pins them in `Cargo.lock`.

## Scope

In scope:

- A new repository `sshtm` (Cargo workspace, edition 2024) containing:
  - crate `sshtm`, with a library (all logic) and a binary (`sshtm`). The
    hidden subcommand `sshtm daemon` runs the supervisor.
  - crate `fake-ssh` (`publish = false`): a stand-in `ssh` binary that the
    default tests put first on `PATH`.
- The CLI: `open`, `ls`, `kill`, `logs`, `hosts`, `shutdown`, `completions`.
- The daemon: a Unix-socket server, the index table, child `ssh` supervision,
  readiness detection, reconnect with backoff, and idle exit.
- Listing host aliases from `~/.ssh/config` (with `Include`), for `hosts` and
  completion.
- CI (GitHub Actions, Linux): fmt, clippy, and the default tests.

Out of scope:

- **TUI.** Dropped. It can return later as a separate front end over the same
  socket protocol. That needs its own plan.
- **Saved tunnels, profiles, machines, any `sshtm` config file.** Dropped for
  good. Zero config is a product goal. Shell aliases cover repeated tunnels.
- **Restoring tunnels after a daemon restart, and autostart.** Dropped.
  Tunnels live exactly as long as the daemon.
- **Importing or migrating Go-version data.** Dropped.
- **Remote (`-R`) and dynamic SOCKS (`-D`) forwards, and non-loopback bind
  addresses.** Dropped for now. They would extend the spec grammar in a later
  plan, after R6.
- **macOS and other platforms.** Dropped for now. Readiness detection and
  parent-death handling are Linux-specific (see Decisions). A port would need
  its own plan.
- **Packaging (AUR, Debian, install scripts, systemd unit).** Ignored for now.
  R5 documents `cargo install --path .` only.
- **Retiring the Go repository.** The user decides that after R6. This plan
  does not touch it beyond holding this file until R0.

## Decisions

| Area | Decision |
| --- | --- |
| SSH backend | Spawn the system `ssh` (found on `PATH`), one child process per tunnel. That gives `~/.ssh/config`, ssh-agent, ProxyJump, FIDO keys and known_hosts exactly as the user has them. Rejected: an in-process client such as `russh` (it would have to rebuild config parsing and auth). |
| Destinations | `<host>` is passed to `ssh` unchanged: a config alias, `user@host`, or `host`. sshtm has no machine store. It parses `~/.ssh/config` only to *list* aliases (`hosts`, completion), and `ssh` itself resolves everything. |
| Forced ssh options | Every child runs `ssh -N -T -o BatchMode=yes -o ExitOnForwardFailure=yes -o ControlMaster=no -o ControlPath=none -o ForkAfterAuthentication=no -L 127.0.0.1:<lport>:<rhost>:<rport> -- <host>`. These are the only places where sshtm overrides the user's config. `ControlMaster=no` is required: in the spike, a `ControlPersist` config made `ssh` hand the forward to a detached master and exit, so the daemon lost the process that owns the tunnel. `ForkAfterAuthentication=no` stops a config `-f` equivalent from detaching the same way. `BatchMode` is required because the daemon has no terminal. |
| Keepalive defaults | Before spawning, the daemon runs `ssh -G -- <host>`. If `serveraliveinterval` is `0` (OpenSSH's default), it adds `-o ServerAliveInterval=15 -o ServerAliveCountMax=3`. If the user set their own values, they win. Without a keepalive, a dead link is never noticed, so reconnect never fires. |
| Host argument safety | Reject a `<host>` that starts with `-` or contains whitespace or control characters. `--` before the host is belt and braces. This prevents option injection such as `-oProxyCommand=…`. |
| Prompts | None. With `BatchMode=yes`, an unknown host key or a key with a passphrase and no agent fails at once. The error shows the last lines of ssh's stderr plus the hint `run 'ssh <host>' once interactively`. |
| Agent | Each `open` request carries the caller's `SSH_AUTH_SOCK`. The daemon stores it with the tunnel and sets it in the child's environment, including on reconnects. Other environment variables come from the daemon's own environment, which it inherited from the CLI that started it. |
| Forward spec | `PORT`, `LPORT:PORT`, `RHOST:PORT`, `LPORT:RHOST:PORT`. `RHOST` defaults to `localhost` (as seen from the SSH server), and `LPORT` defaults to the remote port. In the two-part form, an all-digit first part means `LPORT`. IPv6 hosts use brackets: `[::1]:5432`. `LPORT` `0` means the daemon picks a free port. `ssh -L 0:…` is rejected by OpenSSH ("Bad local forwarding specification"). Bind address is always `127.0.0.1`. |
| One tunnel per spec | `sshtm open db 5432 6379` creates two tunnels, two `ssh` children and two indices. This makes kill-by-index exact. The cost is one SSH connection per forward. |
| Index | When a tunnel is created, it gets the lowest free positive integer. The index never changes while the tunnel exists, including while it reconnects. It is freed only when the tunnel is removed. Rejected: renumbering after each kill (racy with what `ls` just showed), and a counter that only goes up (numbers keep growing). |
| Duplicate local port | `open` fails before spawning if an existing tunnel already holds that local port, and the message names that tunnel's index. A port held by some other program shows up as ssh's bind failure (exit 255, `Address already in use`). |
| Readiness | A tunnel is `up` once `/proc/<pid>/net/tcp{,6}` lists a LISTEN socket on `lport` whose inode is among `/proc/<pid>/fd` socket links. In the spike, readiness was seen 0.13 s after spawn. Rejected: parsing `ssh -v` output (it is not a stable interface) and test-connecting to the port (that opens a real channel to the destination). |
| Initial failure | If the child exits before it is ready, or `READY_TIMEOUT` passes, `open` fails. The child is killed, the index is freed, and the CLI exits 1 and shows the stderr tail. There are no retries before the first `up`. When `LPORT` is `0`, a bind conflict picks a new port up to `AUTO_PORT_ATTEMPTS` times. |
| Reconnect | Once a tunnel has been `up`, any exit of its child sets it to `reconnecting`. It is respawned with exponential backoff (`BACKOFF_MIN` doubling to `BACKOFF_MAX`), and the delay resets after `BACKOFF_RESET` of uptime. It retries until killed. |
| States | `starting`, `up`, `reconnecting` (shows the attempt number and when the next try is). A killed tunnel is removed, not kept as a "stopped" row. |
| Kill | `sshtm kill <idx|range>...` (`2`, `1 3`, `2-4`) and `sshtm kill --all`. Indices that exist are killed. Missing ones are reported, and the CLI then exits 1. Kill sends SIGTERM, waits `KILL_GRACE`, then sends SIGKILL, and returns only once the local port is released. |
| Default command | Bare `sshtm` is the same as `sshtm ls`. |
| CLI surface | `open <host> <spec>...`, `ls [--json]`, `kill`, `logs <idx>` (stderr ring buffer), `hosts`, `shutdown` (kill all, stop the daemon), `completions <shell>`, `--version`. Hidden: `daemon`. |
| Process model | One binary. The daemon is `sshtm daemon`. Only `open` starts it automatically. `ls`, `kill`, `logs` and `shutdown` with no daemon running answer "no tunnels" and do not start one. |
| Daemon lifetime | It exits `IDLE_EXIT` after its table becomes empty. On SIGTERM or SIGINT, and on `shutdown`, it kills all children and removes its socket. Children get `PR_SET_PDEATHSIG(SIGTERM)` and their own process group, so a crashed daemon leaves no orphan `ssh`. |
| Runtime directory | `$XDG_RUNTIME_DIR/sshtm/` (mode `0700`), containing `sock` (mode `0600`), `lock`, and `daemon.log`. If `XDG_RUNTIME_DIR` is unset, use `/tmp/sshtm-<uid>/`, created `0700`. Refuse to use it if it is not owned by the user or is group- or world-accessible. |
| Access control | Directory and socket permissions, plus a `SO_PEERCRED` check that the peer uid equals the daemon's uid. The Go daemon's unauthenticated TCP port is not reproduced. |
| Autostart race | The CLI takes `flock` on `lock`, retries the connect, and only then spawns `current_exe() daemon` (setsid, null stdin and stdout, stderr to `daemon.log`). It waits up to `AUTOSTART_WAIT` for the socket. A daemon that starts and finds a stale `sock` removes it while holding the lock. |
| IPC | One request per connection, newline-delimited JSON with `{"v":1,…}`. A version mismatch makes the CLI fail with `daemon speaks protocol N; run 'sshtm shutdown'`. Rejected: gRPC or protobuf (heavy for about six verbs on a local socket). |
| Output | `ls` prints an aligned table: `IDX HOST LOCAL REMOTE STATE UPTIME`. `--json` prints a stable array of objects with the same fields plus `pid` and `attempt`. Errors go to stderr. Exit code 0 means success, 1 an operation failure, 2 a usage error (clap). |
| Host listing | Parse `Host` lines in `~/.ssh/config` (from `getpwuid`, the same as ssh), following `Include` with `~` and glob expansion (relative paths are resolved against `~/.ssh`, as OpenSSH does). Skip patterns that contain `*`, `?` or `!`. `Match` blocks are ignored. The user's config already uses `Include`. |
| Completion | `clap_complete` dynamic completion (`CompleteEnv`) for bash, zsh and fish. `<host>` completes from `hosts`. Index arguments complete from a running daemon only, never starting one. |
| Crates | `tokio` 1.54 (rt, process, net, signal, time, macros), `clap` 4.6 (derive), `clap_complete` 4.6, `serde` and `serde_json` 1.0, `nix` 0.31 (prctl, flock, peer credentials, signals), `anyhow` 1.0. Dev: `assert_cmd` 2.2 and `tempfile` 3.27. |
| Platform | Linux only. `/proc` readiness and `PR_SET_PDEATHSIG` are the Linux-specific parts. Each sits behind one function so a later port can replace it. |

### Module layout

```text
sshtm/
  Cargo.toml                 workspace: sshtm, fake-ssh
  src/main.rs                clap entry, dispatch
  src/lib.rs
  src/cli.rs                 argument types, output formatting
  src/spec.rs                forward-spec parser
  src/hosts.rs               ~/.ssh/config Host listing
  src/paths.rs               runtime dir resolution and checks
  src/ipc.rs                 request/response types, framing, version
  src/client.rs              connect, autostart, lock
  src/daemon/mod.rs          accept loop, idle exit, signals
  src/daemon/table.rs        index allocation, tunnel records
  src/daemon/supervisor.rs   child lifecycle, backoff, kill
  src/daemon/sshcmd.rs       argv builder, `ssh -G` probe
  src/daemon/ready.rs        /proc listener detection
  fake-ssh/src/main.rs
  docs/plans/rust-rewrite.md
```

### Tunnel lifecycle

1. `open` validates `<host>` and the spec. It allocates an index and checks
   for a duplicate local port under one table lock. The record is `starting`.
2. Probe `ssh -G`, build the argv, and spawn. stderr feeds the ring buffer.
3. Poll readiness every `READY_POLL`. On the first `up`, `open` returns the
   index. If the child exits first, or the timeout passes, the tunnel is
   removed and the stderr tail is returned.
4. If the child exits while `up`, the tunnel goes to `reconnecting`. It sleeps
   the backoff delay, then returns to step 2. Readiness takes it back to `up`.
5. `kill` or `shutdown` cancels any pending backoff, signals the child, waits,
   and removes the record.

### Constants

Timeouts and backoff are product choices. R1 sets them, and R3 revisits them
after daily use. Caps are provisional until R2 measures them.

| Constant | Value | Bounds / notes |
| --- | --- | --- |
| `READY_TIMEOUT` | 20 s | Initial `open` only. It has to exceed a typical `ConnectTimeout` plus ProxyJump hops. If it were near infinite, a hung `open` would block the CLI forever. Revisited in R3. |
| `READY_POLL` | 20 ms | Cost is one `/proc` scan per poll per starting tunnel. |
| `BACKOFF_MIN` / `BACKOFF_MAX` | 1 s / 60 s | A dead host settles at one `ssh` spawn per minute, visible in `ls`. |
| `BACKOFF_RESET` | 60 s of `up` | After that much uptime, the next failure starts again at `BACKOFF_MIN`. |
| `KILL_GRACE` | 2 s | SIGTERM, then SIGKILL. |
| `AUTO_PORT_ATTEMPTS` | 3 | Only for `LPORT` `0`. |
| `IDLE_EXIT` | 30 s | Avoids a stale daemon after an upgrade. If it were near infinite, the daemon would simply stay resident, which is harmless. |
| `AUTOSTART_WAIT` | 2 s | How long the CLI waits for a fresh daemon's socket. |
| `LOG_LINES` | 200 per tunnel | Provisional. R2 measures memory with 100 tunnels. If unbounded, a flapping tunnel's stderr would grow without limit. |
| `MAX_REQUEST_BYTES` | 64 KiB | Provisional. It bounds a malicious or buggy local client. Requests are tiny. |
| Default keepalive | 15 s × 3 | Applied only when `ssh -G` reports interval 0. |

## Testing

Default tests (`cargo test`) need no network, no `sshd` and no extra tools:

- Unit tests: spec grammar (including IPv6 and the two-part rule), index
  allocation, argv building (including the keepalive rule from canned `ssh -G`
  output), backoff schedule, host listing over fixture configs with `Include`,
  globs and wildcards, runtime-dir permission checks, and IPC framing and
  version mismatch.
- Readiness: `ready.rs` is run against the test process's own pid with a
  `std::net::TcpListener` bound to a port, and against a pid without one.
- Integration (`tests/`): runs the real `sshtm` binary with `PATH` starting at
  the `fake-ssh` build directory and `XDG_RUNTIME_DIR` set to a fresh
  `tempfile` directory, so each test gets its own daemon. `fake-ssh` checks
  the forced options. Its behavior is chosen by the host name:
  - `ok` binds the `-L` port and stays up;
  - `fail` writes `Permission denied (publickey).` and exits 255 immediately;
  - `hang` never binds;
  - `flap` binds, then exits after 1 s;
  - `stubborn` binds and ignores SIGTERM;
  - `-G` mode prints canned output, with `serveraliveinterval 0`, or `30` for
    `keepalive`.

  No environment knobs are added to `sshtm` for testing.
- Each integration test ends with `sshtm shutdown` and checks that no
  `fake-ssh` process is left.

Opt-in tests:

| Variable | Enables | Notes |
| --- | --- | --- |
| `SSHTM_E2E_SSHD=1` | Real `ssh` against a throwaway user-mode `sshd` | The harness creates a temp dir with a host key, a client key, `authorized_keys`, `known_hosts`, `sshd_config` (port from an ephemeral bind, `UsePAM no`, `StrictModes no`) and `ssh_config`. It starts `/usr/bin/sshd -D -f …` plus a local echo listener. `ssh` ignores `$HOME` (spike 3), so the harness puts a wrapper `ssh` first on `PATH` that execs `/usr/bin/ssh -F <tmp>/ssh_config "$@"`. Teardown kills `sshd` and its session processes by process group and removes the temp dir, even on panic (drop guard). No root. Nothing outside the temp dir is touched. |

A phase with opt-in tests is complete only when its status records a passing
`SSHTM_E2E_SSHD=1 cargo test -- --include-ignored e2e` run.

**Spike 1: user-mode sshd and readiness (2026-10-24).** Setup: a temp dir with
ed25519 host and client keys, `sshd -f` on 127.0.0.1:22299 as an unprivileged
user, and `ssh -F` against it, with `python3 -m http.server` as the
destination. Observed:
- Login works without root.
- `ssh -v` logs `Local forwarding listening on 127.0.0.1 port N` only after
  `Authenticated`.
- `ss -ltnp` attributes the LISTEN socket to the `ssh` pid, and a Python
  `/proc/<pid>/fd` plus `/proc/net/tcp` scan saw it 0.13 s after spawn.
- `curl` through the tunnel returned 200.
- SIGTERM to `ssh` released the port.
- A second `ssh` on the same local port with `ExitOnForwardFailure=yes`
  exited 255 at once with `Address already in use`.
- `-L 127.0.0.1:0:…` is rejected as "Bad local forwarding specification".

Consequences: the readiness design, the `LPORT` `0` handling, and the
opt-in harness above.

**Spike 2: dead server detection (2026-10-24).** With
`ServerAliveInterval=2`/`CountMax=2`, the `sshd-session` processes were
SIGSTOPped: `ssh` exited 255 with
`Timeout, server 127.0.0.1 not responding.` within 12 s. Stopping only
the listener and the privileged session process was not enough, because the
unprivileged `sshd-session` child kept answering. The opt-in harness must
stop or kill the whole process group. `ssh -G` reports
`serveraliveinterval 0` by default. Consequence: the keepalive-defaults
decision.

**Spike 3: multiplexing and `$HOME` (2026-10-24).** With
`ControlMaster auto` and `ControlPersist 60`, `ssh -N -L` forked a detached
master that owned the listener, and the spawned pid exited. SIGTERM to it did
nothing to the forward. With `-o ControlMaster=no -o ControlPath=none`, the
spawned pid owned the listener. `HOME=/tmp ssh -v` still read
`/home/luis/.ssh/config`. Consequences: the forced ssh options, and the
`PATH` wrapper in the e2e harness.

All spike processes and `/tmp/sshtm-spike-*` were removed. No listeners
remained on the spike ports.

## Phases

| Phase | Builds | Detailed in |
| --- | --- | --- |
| R0 | New repo, workspace skeleton, CI, `fake-ssh`, e2e harness | this plan |
| R1 | Spec parser, argv builder, readiness, single-tunnel supervisor | this plan |
| R2 | Daemon, socket, IPC, index table, autostart, idle exit | this plan |
| R3 | CLI verbs `open`, `ls`, `kill`, `logs`, `shutdown`, output | this plan |
| R4 | Host listing and shell completion | this plan |
| R5 | Hardening: signals, orphans, permissions, measurements | this plan |
| R6 | README, man-style help text, 0.1.0 tag | this plan |

### R0 — Repository and harness

**Status:** complete (implementation and local verification, 2026-10-24).
The edition-2024 workspace, library/binary module stubs, Rust 1.99.0 pin,
`Cargo.lock`, Linux CI, all fake-SSH behaviors, and opt-in user-mode sshd
harness are implemented. Both workspace members are default members, so plain
`cargo test` exercises fake-SSH too. The `sshtm` binary deliberately reports
“not implemented yet” until the later engine/CLI phases. Files, including the
lockfile, remain uncommitted; no commit or GitHub Actions run was performed.

Verification on Linux with rustc 1.99.0, cargo 1.99.0 and OpenSSH 10.6p1:

- `cargo fetch` populated the dependency cache and generated the lockfile.
- `cargo fmt --all --check` passed.
- `cargo clippy --offline --locked --workspace --all-targets -- -D warnings`
  passed.
- `cargo test --offline --locked` passed: nine fake-SSH tests, three ignored
  e2e entries. This covers listening, failure within 100 ms, config probing,
  hang, flap, SIGTERM resistance, and forced-option validation.
- `SSHTM_E2E_SSHD=1 cargo test --offline --locked -- --include-ignored e2e`
  passed: success and deliberate-panic cleanup self-tests plus the internal
  reaper entry point. Each self-test checks `echo ok` through the PATH wrapper,
  the echo listener, session/process-group removal, and tempdir removal.
- A fresh temporary source copy, without `target/` or `.git/`, passed
  `CARGO_NET_OFFLINE=true cargo test --locked` using only the fetched cache.
  This substitutes for a clean clone while these files are uncommitted; the
  temporary source/build directory was removed afterward.

#### Post-implementation review hardening

- Session processes can escape the sshd listener's process group. A dedicated
  subprocess subreaper now freezes the descendant tree until stable, kills
  without resuming shells, and reaps descendants. Success and panic tests
  exercise a shell poised to launch background work after its first sleep.
- The server sets `PermitUserRC no`, verified with `sshd -T`. The fixture is
  not a sandbox: the account's login-shell startup hooks and system SSH hooks
  remain in effect; use a trusted test account. Fixture-owned files remain
  under its tempdir, and no global environment or system config is changed.
- Fake-SSH rejects conflicting forced options in either order and rejects
  noncanonical `-o` whitespace syntax rather than accidentally accepting a
  value that would override a required option in real OpenSSH.
- Only the deliberate fixture-body panic is caught, with its exact payload
  checked. Cleanup assertions execute outside that catch. Temporarily forcing
  the marker assertion to fail made `e2e_sshd_cleanup_panic` fail as expected;
  the fault injection was removed and the complete opt-in suite passed again.
- Independent review rechecked all findings and found no remaining issues.

**Builds:**

- The workspace and module layout above (stub modules) in this repository
  (`~/dev/sshtm`, initialized 2026-10-24 with only this plan),
  `rust-toolchain.toml` pinned to stable 1.99, `rust-version = "1.99"`, and a
  committed `Cargo.lock`.
- `fake-ssh` with the host-name behaviors listed in Testing.
- The e2e harness module (`tests/support/sshd.rs`), gated on `SSHTM_E2E_SSHD`.
- GitHub Actions: `cargo fmt --check`, `cargo clippy -- -D warnings`,
  `cargo test`.

**Exit criteria:**

- `cargo test` passes on a clean clone with no network after
  `cargo fetch`.
- A harness self-test starts `sshd`, runs `ssh … echo ok` through the
  wrapper, gets `ok`, and after the test no `sshd` from the temp dir is running
  and the temp dir is gone. The same holds when the test body panics
  (`SSHTM_E2E_SSHD`).
- `fake-ssh ok` started with `-L 127.0.0.1:P:localhost:1` accepts a TCP
  connect on P. `fake-ssh fail` exits 255 within 100 ms.

### R1 — Engine

**Status:** not started.

**Builds:**

- `spec.rs`: parse to `Forward { local: Option<u16> /* None = auto */, rhost, rport }`.
- `sshcmd.rs`: host validation, the `ssh -G` probe, and the argv builder.
- `ready.rs`: `listening(pid, port) -> bool` from `/proc`.
- `supervisor.rs`: spawn with `PR_SET_PDEATHSIG`, a process group, null
  stdin, and piped stderr into a `LOG_LINES` ring. It has a state machine
  (`starting`, `up`, `reconnecting`), backoff, and `kill()` with
  `KILL_GRACE`. It runs as a library type, without a daemon yet.

**Exit criteria:**

- The spec table test passes. Some cases:
  - `5432` gives local 5432, `localhost:5432`.
  - `15432:5432` gives 15432, `localhost:5432`.
  - `db:5432` gives 5432, `db:5432`.
  - `0:db:5432` gives auto.
  - `[::1]:5432` gives 5432, `::1:5432`.
  - `70000`, `a:b:c:d` and `` are errors.
- A host of `-oProxyCommand=x`, `a b` or `""` is rejected before any spawn.
  `fake-ssh` records that it was never executed.
- The argv contains every forced option. The keepalive options appear for
  canned `-G` with interval 0 and are absent for interval 30.
- `listening` is true for the test's own bound listener and false after it
  is dropped.
- Supervisor against `fake-ssh ok`: `up` within 1 s. Against `fail`: an
  error containing `Permission denied` and no live child. Against `hang`:
  an error after `READY_TIMEOUT` (shortened through a constructor parameter
  in tests) and no live child.
- Against `flap`: it goes `up`, then `reconnecting` with attempt 1 and a 1 s
  delay, then `up` again. The delays seen follow 1, 2, 4 s. The index is not
  involved yet.
- `kill()` on a child that ignores SIGTERM (`fake-ssh` mode `stubborn`)
  returns after about `KILL_GRACE`, and the port is free.
- (`SSHTM_E2E_SSHD`) The supervisor opens a real forward to the echo
  listener, and bytes round-trip. When the server's process group is
  SIGSTOPped, the tunnel reaches `reconnecting` within 15 s with test
  keepalives of 2 × 2. After SIGCONT, it returns to `up`.

### R2 — Daemon and IPC

**Status:** not started.

**Builds:**

- `paths.rs`, `ipc.rs` (requests `Open`, `List`, `Kill`, `Logs`, `Shutdown`,
  `Ping`, with `v`), `table.rs` (lowest-free index), `daemon/mod.rs` (accept
  loop, peer-uid check, idle exit, signal handling), and `client.rs`
  (connect, `flock`, autostart).

**Exit criteria:**

- Index allocation: open three, kill 2, open one, and it gets 2. Kill 1 and 2,
  and 3 keeps index 3 during and after a `flap` reconnect.
- Ten concurrent `open` requests with distinct ports get indices 1 to 10 with
  no duplicates. Two concurrent `open` on the same port: exactly one
  succeeds, and the other names the winner's index.
- Ten concurrent CLI processes with no daemon running end up with exactly one
  daemon process, and all ten requests are answered.
- A stale `sock` file with no daemon is replaced, and `open` succeeds.
- The runtime dir is `0700` and `sock` is `0600`. A pre-existing dir with mode
  `0755` makes the CLI refuse with a message that names the path.
- A request with `v: 99` gets a version error. A 1 MiB request line is
  rejected and the daemon keeps serving.
- The daemon exits `IDLE_EXIT` after the last kill (shortened in tests
  through a hidden `daemon --idle-exit` flag) and removes `sock`.
- SIGTERM to the daemon: all children are gone and their ports are free
  within `KILL_GRACE` plus 1 s. SIGKILL to the daemon: the children are gone
  within 1 s (pdeathsig).
- Memory with 100 `ok` tunnels and full log rings is recorded here, and
  `LOG_LINES` is confirmed or changed.

### R3 — CLI

**Status:** not started.

**Builds:**

- clap definitions for every verb, bare `sshtm` running `ls`, table and JSON
  output, the exit-code rules, and the error message formats (including the
  stderr tail and the interactive-ssh hint).

**Exit criteria:**

- `sshtm open ok 5432 6379` prints two lines, each with an index, the local
  address and the remote address, and exits 0. `sshtm` then lists both as
  `up`.
- `sshtm open fail 5432` exits 1, prints `Permission denied (publickey).` and
  the hint, and `sshtm ls` shows no row for it.
- `sshtm kill 1 3` with only 1 present kills 1, reports `no tunnel 3`, and
  exits 1. `sshtm kill 2-4` kills those present. `sshtm kill --all` leaves the
  table empty.
- `sshtm ls`, `kill`, `logs` and `shutdown` with no daemon print "no tunnels"
  (or nothing for `shutdown`), exit 0 (`kill` exits 1 for a named index), and
  start no daemon.
- `sshtm ls --json` output parses, and its field names match a committed
  snapshot.
- `sshtm logs N` shows the `fake-ssh` stderr lines in order.
- `sshtm open ok 0` reports the chosen port, and a connect to it succeeds.
- (`SSHTM_E2E_SSHD`) `sshtm open spike 18081`, a round-trip through the
  port, `sshtm kill 1`, and the port is free.
- Constants `READY_TIMEOUT` and the backoff values are reviewed after a week
  of daily use, and the result is recorded in this phase's status.

### R4 — Hosts and completion

**Status:** not started.

**Builds:**

- `hosts.rs`, `sshtm hosts`, and `sshtm completions <bash|zsh|fish>` using
  `CompleteEnv`, with completers for host and index arguments.

**Exit criteria:**

- On a fixture tree, `hosts` returns exactly the concrete aliases, sorted
  and deduplicated:
  - `Host a b`;
  - `Include conf.d/*`, with two files;
  - `Include ~/abs`;
  - wildcard, `!neg` and `Match` blocks are excluded;
  - a missing include file is ignored;
  - an include cycle ends.
- Running the completion entry with the fish protocol for `sshtm open <TAB>`
  returns the fixture aliases. `sshtm kill <TAB>` with a daemon holding
  tunnels 1 and 2 returns `1` and `2` with host descriptions. With no daemon
  it returns nothing and starts none.
- `sshtm completions fish | fish -n` (fish syntax check) passes when `fish`
  is on `PATH`. The same goes for `bash -n` and `zsh -n`.

### R5 — Hardening

**Status:** not started.

**Builds:**

- A review pass over signal handling, fd leaks (no inherited sockets in
  children: check `/proc/<child>/fd`), and the permission checks.
  `daemon.log` is rotated or truncated at daemon start.
- `cargo install --path .` documented as the install path.

**Exit criteria:**

- A child `ssh` has only fds 0, 1 and 2 plus what ssh opens itself. It
  has no daemon listener or client socket fd.
- After 1,000 open and kill cycles against `fake-ssh ok`, daemon RSS and fd
  counts have not grown. The figures are recorded.
- With `XDG_RUNTIME_DIR` unset, `/tmp/sshtm-<uid>` is created `0700`. If that
  path is pre-created by another uid, or is a symlink, the CLI refuses.
- (`SSHTM_E2E_SSHD`) The full opt-in suite passes. The run is recorded with
  the OpenSSH version.

### R6 — Docs and release

**Status:** not started.

**Builds:**

- `README.md`: purpose, zero-config statement, the commands, the spec
  grammar, the forced ssh options and why, troubleshooting (host key,
  passphrase, agent).
- `--help` text for each verb, with examples. Tag `v0.1.0`.

**Exit criteria:**

- Every command and flag in the README appears in `sshtm --help` or
  `sshtm <verb> --help`. A test compares the two lists.
- A fresh clone, `cargo install --path .`, and `sshtm open <alias> <port>`
  against a real host from the user's config give a working tunnel. This is
  recorded manually in the status.

## Risks

- **`/proc` readiness on unusual setups.** In network namespaces or with
  `hidepid`, ssh's sockets may not be visible. The daemon reads
  `/proc/<pid>/net/tcp`, which shows the child's own namespace, and its own
  children are always visible to it. If this fails, `open` times out with a
  clear message instead of reporting false success.
- **User config that defeats supervision.** Multiplexing and
  `ForkAfterAuthentication` are overridden. A `ProxyCommand` or `LocalCommand`
  that daemonizes something is not, and R6 documents it.
- **Remote destination unreachable.** `ExitOnForwardFailure` covers only the
  local bind. A wrong `RHOST:RPORT` shows as `up`, and each connection fails
  with `channel N: open failed` in `logs`. `ls` does not try to detect it. R6
  documents it.
- **Agent socket churn.** A stored `SSH_AUTH_SOCK` can go stale after the
  user logs out and back in. Reconnects then fail with a publickey error that
  is visible in `logs`. Fix: `kill` and `open` again.
