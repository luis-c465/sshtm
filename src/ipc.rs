//! Versioned request/response types and bounded JSON framing.
use std::ffi::OsString;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

pub const VERSION: u32 = 1;
pub const MAX_REQUEST_BYTES: usize = 65536;
// Logs returns one ring: 200 × 4096 input bytes × 6 for worst-case JSON
// escaping is under 5 MiB. Leave headroom for metadata and other responses.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Open {
        host: String,
        forward: crate::spec::Forward,
        auth_sock: Option<OsString>,
    },
    List,
    Kill {
        indices: Vec<u32>,
        all: bool,
    },
    Logs {
        index: u32,
    },
    Shutdown,
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    #[serde(flatten)]
    pub result: ResponseResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseResult {
    Ok { data: serde_json::Value },
    Error { message: String },
}

impl Request {
    pub fn new(command: Command) -> Self {
        Self {
            v: VERSION,
            command,
        }
    }
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            v: VERSION,
            result: ResponseResult::Ok { data },
        }
    }
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            v: VERSION,
            result: ResponseResult::Error {
                message: message.into(),
            },
        }
    }
}

async fn read_json<T: DeserializeOwned>(stream: &mut UnixStream, limit: usize) -> Result<T> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let count = stream.read(&mut chunk).await?;
        ensure!(count != 0, "connection closed before newline");
        let end = chunk[..count].iter().position(|byte| *byte == b'\n');
        let length = end.unwrap_or(count);
        ensure!(
            bytes.len() + length <= limit,
            "JSON frame exceeds {limit} bytes"
        );
        bytes.extend_from_slice(&chunk[..length]);
        if end.is_some() {
            return Ok(serde_json::from_slice(&bytes)?);
        }
    }
}

async fn write_json<T: Serialize>(stream: &mut UnixStream, value: &T, limit: usize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= limit, "JSON frame exceeds {limit} bytes");
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    Ok(())
}

/// Version validation belongs to the server so it can return a protocol error.
pub async fn read_request(stream: &mut UnixStream) -> Result<Request> {
    read_json(stream, MAX_REQUEST_BYTES).await
}
pub async fn write_request(stream: &mut UnixStream, request: &Request) -> Result<()> {
    write_json(stream, request, MAX_REQUEST_BYTES).await
}
pub async fn read_response(stream: &mut UnixStream) -> Result<Response> {
    let response: Response = read_json(stream, MAX_RESPONSE_BYTES).await?;
    ensure!(
        response.v == VERSION,
        "daemon speaks protocol {}; run 'sshtm shutdown'",
        response.v
    );
    Ok(response)
}
pub async fn write_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
    write_json(stream, response, MAX_RESPONSE_BYTES).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn framing_and_version() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        write_request(&mut a, &Request::new(Command::Ping))
            .await
            .unwrap();
        assert!(matches!(
            read_request(&mut b).await.unwrap().command,
            Command::Ping
        ));
        write_response(
            &mut b,
            &Response {
                v: 99,
                result: ResponseResult::Ok {
                    data: serde_json::Value::Null,
                },
            },
        )
        .await
        .unwrap();
        assert!(
            read_response(&mut a)
                .await
                .unwrap_err()
                .to_string()
                .contains("daemon speaks protocol 99")
        );
    }

    #[tokio::test]
    async fn bounded_and_unterminated() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let writer = tokio::spawn(async move {
            let _ = a.write_all(&vec![b'x'; MAX_REQUEST_BYTES + 1]).await;
        });
        assert!(read_request(&mut b).await.is_err());
        writer.await.unwrap();
        let (a, mut b) = UnixStream::pair().unwrap();
        drop(a);
        assert!(read_request(&mut b).await.is_err());
    }

    #[tokio::test]
    async fn worst_case_full_log_ring_fits_response_bound() {
        use crate::daemon::supervisor::{LOG_LINE_BYTES, LOG_LINES};
        let lines = vec!["\0".repeat(LOG_LINE_BYTES); LOG_LINES];
        let response = Response::ok(serde_json::json!({ "lines": lines }));
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let (written, read) =
            tokio::join!(write_response(&mut a, &response), read_response(&mut b));
        written.unwrap();
        assert!(
            matches!(read.unwrap().result, ResponseResult::Ok { data } if data["lines"].as_array().unwrap().len() == LOG_LINES)
        );
    }

    #[test]
    fn agent_socket_round_trip() {
        let request = Request::new(Command::Open {
            host: "host".into(),
            forward: "5432".parse().unwrap(),
            auth_sock: Some(OsString::from("/tmp/agent")),
        });
        let bytes = serde_json::to_vec(&request).unwrap();
        let decoded: Request = serde_json::from_slice(&bytes).unwrap();
        assert!(matches!(
            decoded.command,
            Command::Open {
                auth_sock: Some(_),
                ..
            }
        ));
    }
}
