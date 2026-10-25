//! Local-forward specification parsing.
use std::{net::Ipv6Addr, str::FromStr};

use anyhow::{Result, bail, ensure};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Forward {
    pub local: Option<u16>,
    pub rhost: String,
    pub rport: u16,
}

fn port(value: &str, remote: bool) -> Result<u16> {
    ensure!(
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
        "invalid port: {value:?}"
    );
    let port = value.parse::<u16>()?;
    ensure!(!remote || port != 0, "remote port must be nonzero");
    Ok(port)
}

impl FromStr for Forward {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        // Split only outside IPv6 brackets, so bare IPv6 is never ambiguous.
        let mut parts = Vec::new();
        let mut bracket = false;
        let mut start = 0;
        for (i, c) in value.char_indices() {
            match c {
                '[' if !bracket && i == start => bracket = true,
                ']' if bracket => bracket = false,
                '[' | ']' => bail!("invalid brackets in forward specification"),
                ':' if !bracket => {
                    parts.push(&value[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        ensure!(!bracket, "unclosed IPv6 brackets");
        parts.push(&value[start..]);
        let (local, host, remote) = match parts.as_slice() {
            [remote] => (None, "localhost", *remote),
            [first, remote] if !first.is_empty() && first.bytes().all(|b| b.is_ascii_digit()) => {
                (Some(*first), "localhost", *remote)
            }
            [host, remote] => (None, *host, *remote),
            [local, host, remote] => (Some(*local), *host, *remote),
            _ => bail!("invalid forward specification: {value:?}"),
        };
        let rhost = if let Some(inner) = host.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            inner.parse::<Ipv6Addr>()?;
            inner
        } else {
            ensure!(
                !host.is_empty()
                    && !host.chars().any(|c| c.is_whitespace()
                        || c.is_control()
                        || matches!(c, ':' | '[' | ']')),
                "invalid remote host"
            );
            host
        };
        let rport = port(remote, true)?;
        let local = local.map(|s| port(s, false)).transpose()?.unwrap_or(rport);
        Ok(Self {
            local: (local != 0).then_some(local),
            rhost: rhost.into(),
            rport,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar() {
        for (input, local, host, remote) in [
            ("5432", Some(5432), "localhost", 5432),
            ("15432:5432", Some(15432), "localhost", 5432),
            ("db:5432", Some(5432), "db", 5432),
            ("0:db:5432", None, "db", 5432),
            ("0:5432", None, "localhost", 5432),
            ("[::1]:5432", Some(5432), "::1", 5432),
            ("1234:[2001:db8::1]:65535", Some(1234), "2001:db8::1", 65535),
        ] {
            assert_eq!(
                input.parse::<Forward>().unwrap(),
                Forward {
                    local,
                    rhost: host.into(),
                    rport: remote
                }
            );
        }
    }

    #[test]
    fn invalid_specs() {
        for value in [
            "",
            "70000",
            "0",
            "a:b:c:d",
            ":80",
            "1::80",
            "db:0",
            "-1:db:80",
            "1:db:65536",
            "[::1:80",
            "::1:80",
            "[db]:80",
            "db x:80",
            "db\n:80",
            "1:db:",
            "1:[::1]x:80",
        ] {
            assert!(value.parse::<Forward>().is_err(), "{value:?}");
        }
    }
}
