# sshtm

A small Linux command-line supervisor for local SSH port forwards. It runs the
system OpenSSH client with your existing SSH configuration: **zero sshtm
configuration**, no profiles, no saved tunnels, and no separate service to set up.
SSH aliases, ssh-agent, ProxyJump, FIDO keys, and known_hosts work through OpenSSH.

## Install

Requires Linux, `ssh` on `PATH`, and Rust 1.99 or newer. From this repository:

```sh
cargo install --path .
```

Put `~/.cargo/bin` on `PATH`. See [installation and upgrades](docs/install.md).
This Rust rewrite is not compatible with the former Go tool or its stored data.

## Quick start

Replace `my-host` with an alias from your SSH config or a destination such as
`user@example.com`. Authenticate and accept its host key interactively first:

```sh
ssh my-host
sshtm open my-host 5432 6379
sshtm ls
sshtm kill 1
sshtm shutdown
```

Each spec creates a separate SSH connection and prints its index, local address,
and remote address. Connect your application to the printed `127.0.0.1` port.
The remote address is reached **from the SSH server**, not your laptop.

## Commands

Bare `sshtm` is `sshtm ls`. Use `sshtm --help` or `sshtm <command> --help`
for details and examples.

<!-- cli-reference:start -->
| Command | Arguments and flags | Purpose |
| --- | --- | --- |
| `open` | `<HOST> <SPECS>...` | Open one tunnel per spec; wait for local readiness. |
| `ls` | `--json` | List tunnels as an aligned table or stable JSON array. |
| `kill` | `<INDICES>...`, `--all` | Kill indices or inclusive ranges, or all tunnels. |
| `logs` | `<INDEX>` | Print recent SSH stderr; not a live stream. |
| `hosts` | | List sorted, unique concrete aliases from SSH config and Includes. |
| `completions` | `<SHELL>` | Generate dynamic bash, zsh, or fish registration. |
| `shutdown` | | Kill all tunnels and stop the daemon. |
| `help` | `[COMMAND]...` | Show help for a command (also available via `--help`). |

Common option: `-h`, `--help` (except the help dispatcher). Top-level only: `-V`, `--version`.
<!-- cli-reference:end -->

```sh
sshtm ls --json
sshtm kill 1 3
sshtm kill 2-4
sshtm kill --all
sshtm logs 1
sshtm hosts
sshtm --version
```

`kill` waits for child exit and local port release, escalating from SIGTERM to
SIGKILL after two seconds. Existing indices are killed even if others are
missing; missing indices cause exit 1. A request accepts at most 4,096 distinct
indices; use separate requests or `--all` for larger batches.

Exit codes: **0** success, **1** operation failure, **2** usage error. Errors go
to stderr. Multiple specs are attempted in order; successful tunnels remain if
another spec fails, and the command exits 1.

`ls` columns are `IDX HOST LOCAL REMOTE STATE UPTIME`. States are `starting`,
`up`, and `reconnecting` (with attempt and retry delay). JSON rows contain
`idx`, `host`, `local`, `remote`, `state`, `uptime`, `pid`, `attempt`, and
`next_retry_ms`. Uptime is seconds in the current up period; retry delay is
milliseconds or null, and pid is null when no forward child is running.
Indices remain fixed while reconnecting and become reusable only after removal.

## Forward specs

| Form | Example | Local → remote |
| --- | --- | --- |
| `PORT` | `5432` | `127.0.0.1:5432` → `localhost:5432` |
| `LPORT:PORT` | `15432:5432` | `127.0.0.1:15432` → `localhost:5432` |
| `RHOST:PORT` | `db.internal:5432` | `127.0.0.1:5432` → `db.internal:5432` |
| `LPORT:RHOST:PORT` | `15432:db.internal:5432` | `127.0.0.1:15432` → `db.internal:5432` |

In a two-part spec, an all-digit first field means a **local port**, not a
hostname. Remote ports must be 1–65535. Local port `0` chooses a free port:

```sh
sshtm open my-host 0:5432
sshtm open my-host 0:db.internal:5432
sshtm open my-host '[::1]:5432'
sshtm open my-host '15432:[::1]:5432'
```

Bare `0` is invalid: it would mean remote port zero. Bracket IPv6 addresses and
quote them against shell glob expansion. All local listeners bind to IPv4
loopback (`127.0.0.1`); non-loopback, remote forwards, and SOCKS are unsupported.

## SSH behavior and lifetime

Only `open` automatically starts the background daemon. Listing, killing,
reading logs, shutdown, and completion never start it. With no daemon, `ls`
prints `no tunnels` (`--json`: `[]`), `logs` prints `no tunnels`, and `shutdown`
succeeds silently. Killing a named missing index fails.

Before first readiness, failure or a 20-second readiness timeout fails `open`
and removes the tunnel; there is no reconnect yet. A separate 20-second limit
bounds the preceding `ssh -G` config probe. Once up, a tunnel reconnects after
any child exit, with delays doubling from 1 to 60 seconds. After 60 seconds up,
the delay resets. Retries continue until killed. Local automatic-port bind
conflicts may select a new port, up to three attempts.

Tunnels live only as long as the daemon: **nothing is saved or restored**.
An empty daemon exits after 30 seconds. Shutdown and termination signals clean
up children. Before upgrading, run `sshtm shutdown`, install again, and reopen
your tunnels. There is no autostart service.

Each forward invokes the equivalent of:

```text
ssh -N -T -o BatchMode=yes -o ExitOnForwardFailure=yes \
  -o ControlMaster=no -o ControlPath=none -o ForkAfterAuthentication=no \
  -L 127.0.0.1:<lport>:<rhost>:<rport> -- <host>
```

- `-N -T`: no remote command or terminal.
- `BatchMode=yes`: no authentication prompts in the background.
- `ExitOnForwardFailure=yes`: fail if the local listener cannot bind.
- `ControlMaster=no`, `ControlPath=none`, `ForkAfterAuthentication=no`: keep
  the listener owned by the supervised process, not a detached SSH master.

The daemon probes `ssh -G -- <host>`. If `serveraliveinterval` is zero, it adds
`ServerAliveInterval=15` and `ServerAliveCountMax=3` to detect dead links.
Otherwise your configured keepalive values win. Each open stores that caller's
`SSH_AUTH_SOCK` for initial connection and reconnects. Other environment values
come from the process that started the daemon.

OpenSSH reads and resolves your connection configuration. The `hosts` command
only lists aliases: it uses the current uid's passwd home (not `$HOME`), follows
`Include` with tilde/glob expansion relative to `~/.ssh`, skips wildcard and
negated names and Match blocks, and never executes config helpers.

Runtime files are private: `$XDG_RUNTIME_DIR/sshtm/`, or `/tmp/sshtm-<uid>/`
when XDG_RUNTIME_DIR is unset, mode 0700. They contain `sock` (0600), `lock`, and
`daemon.log` (truncated at daemon start). Unsafe ownership or permissions are
refused; peers must have the same uid. This is local IPC, not a TCP service.

## Shell completion

Run the matching registration in your shell, or add it to its startup file:

```bash
source <(sshtm completions bash)
```

```zsh
source <(sshtm completions zsh)
```

```fish
sshtm completions fish | source
```

Regenerate registration after upgrades. Hosts come from the alias reader;
kill/log indices come only from an already running daemon.

## Troubleshooting

- **Unknown host key / passphrase / authentication failure:** run `ssh my-host`
  interactively and resolve the error, then retry. Load encrypted keys into
  ssh-agent; the daemon cannot prompt. Check `SSH_AUTH_SOCK` in your caller.
- **Agent changed after login:** the old stored socket may be stale. Kill the
  affected tunnel and reopen it from the new session.
- **Address already in use:** choose another local port or use `0:PORT`.
  A port reserved by sshtm reports its tunnel index; inspect `sshtm ls`.
- **Up, but connections fail:** up means the SSH child owns a local listener,
  not that the remote service is reachable. Check `sshtm logs 1` for channel
  errors and verify the remote host/port from the SSH server. No test connection
  is made during readiness detection.
- **Readiness timeout:** check authentication, ProxyJump, and SSH configuration.
  Linux procfs must expose the child's fds and TCP listeners; unusual procfs
  restrictions or namespaces can prevent detection.
- **Helpers outlive SSH:** custom `ProxyCommand` or `LocalCommand` programs that
  daemonize themselves are outside sshtm's supervision. Avoid detaching helpers.
- **Protocol mismatch after upgrade:** run `sshtm shutdown` as the error suggests
  before reopening. If an incompatible daemon refuses shutdown, terminate that
  daemon with SIGTERM; do not simply remove a live daemon's socket.

Logs retain the last 200 stderr chunks per tunnel (each at most 4 KiB input);
long lines are split and older diagnostics are discarded. Use `daemon.log`
for daemon-level errors.

## Development

```sh
cargo fmt --all --check
cargo clippy --offline --locked --workspace --all-targets -- -D warnings
cargo test --offline --locked
SSHTM_E2E_SSHD=1 cargo test --offline --locked -- --include-ignored e2e
```

Run `cargo fetch` first if the dependency cache is empty. Default tests use
`fake-ssh`, need no network or sshd, and isolate runtime paths. Opt-in e2e tests
require OpenSSH server tools and a trusted local test account; they create a
throwaway user-mode sshd and keys, not a system service. See the
[implementation plan](docs/plans/rust-rewrite.md) for scope and verification.
