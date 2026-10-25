# Installation

sshtm currently supports Linux only and requires the system OpenSSH client
(`ssh` on `PATH`). From this repository, with the Rust toolchain installed:

```sh
cargo install --path .
```

Ensure Cargo's binary directory (normally `~/.cargo/bin`) is on `PATH`.
sshtm uses your existing SSH configuration; it has no configuration file of its
own. Authenticate and accept the host key interactively first if necessary:

```sh
ssh my-host
sshtm open my-host 5432
sshtm ls
sshtm kill 1
sshtm shutdown
```

Tunnels are not restored after shutdown or a daemon restart. Before upgrading,
run `sshtm shutdown`, then repeat the install command and reopen your tunnels.
