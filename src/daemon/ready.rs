//! Linux procfs listener readiness detection.
use std::{collections::HashSet, fs, path::Path};

/// Require both a loopback LISTEN entry and a socket owned by this process.
/// Missing or inaccessible procfs state is not evidence of readiness.
pub fn listening(pid: u32, port: u16) -> bool {
    if port == 0 {
        return false;
    }
    let root = format!("/proc/{pid}");
    let Ok(entries) = fs::read_dir(Path::new(&root).join("fd")) else {
        return false;
    };
    let inodes: HashSet<u64> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let target = fs::read_link(entry.path()).ok()?;
            target
                .to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')?
                .parse()
                .ok()
        })
        .collect();
    ["tcp", "tcp6"].iter().any(|table| {
        fs::read_to_string(Path::new(&root).join("net").join(table))
            .is_ok_and(|contents| table_listening(&contents, port, &inodes))
    })
}

fn table_listening(contents: &str, port: u16, inodes: &HashSet<u64>) -> bool {
    contents.lines().skip(1).any(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 10 || fields[3] != "0A" {
            return false;
        }
        let Some((address, hex_port)) = fields[1].split_once(':') else {
            return false;
        };
        // procfs formats addresses as native-endian 32-bit words.
        let v4 = format!("{:08X}", u32::from_ne_bytes([127, 0, 0, 1]));
        let v6 = format!(
            "000000000000000000000000{:08X}",
            u32::from_ne_bytes([0, 0, 0, 1])
        );
        (address == v4 || address == v6)
            && u16::from_str_radix(hex_port, 16).ok() == Some(port)
            && fields[9]
                .parse::<u64>()
                .is_ok_and(|inode| inodes.contains(&inode))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn own_listener() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(listening(std::process::id(), port));
        assert!(!listening(u32::MAX, port));
        drop(listener);
        assert!(!listening(std::process::id(), port));
    }
    #[test]
    fn ipv6_listener() {
        let Ok(listener) = TcpListener::bind(("::1", 0)) else {
            return;
        };
        assert!(listening(
            std::process::id(),
            listener.local_addr().unwrap().port()
        ));
    }
    #[test]
    fn wildcard_is_not_ready() {
        let listener = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        assert!(!listening(
            std::process::id(),
            listener.local_addr().unwrap().port()
        ));
    }
    #[test]
    fn table_requires_address_state_port_and_owner() {
        let owned = HashSet::from([123]);
        let address = format!("{:08X}", u32::from_ne_bytes([127, 0, 0, 1]));
        let row = format!("header\n0: {address}:1538 00000000:0000 0A 0:0 0:0 0 1000 0 123\n");
        assert!(table_listening(&row, 5432, &owned));
        assert!(!table_listening(&row, 5433, &owned));
        assert!(!table_listening(&row, 5432, &HashSet::new()));
        assert!(!table_listening(&row.replace(" 0A ", " 01 "), 5432, &owned));
        assert!(!table_listening(
            &row.replace(&address, "00000000"),
            5432,
            &owned
        ));
        assert!(!table_listening("header\nmalformed", 5432, &owned));
    }
}
