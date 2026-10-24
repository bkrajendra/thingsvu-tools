// SPDX-License-Identifier: GPL-3.0-or-later
//! Native NNG REQ0 client over the NNG ipc transport.
//!
//! KiCad's API server is an NNG REP0 socket listening on `ipc://<socket path>`. Only the subset
//! needed by a single-request-at-a-time REQ client is implemented:
//!
//! 1. Connection: Windows named pipe `\\.\pipe\<socket path>`, or a Unix domain socket.
//! 2. Handshake: each side sends the 8-byte SP header `00 'S' 'P' 00 <protocol u16 BE> 00 00`
//!    (REQ0 = 0x30, REP0 = 0x31).
//! 3. Messages: `0x01`, then the body length as u64 big-endian, then the body. A REQ body starts
//!    with a 4-byte big-endian request ID with the high bit set; the reply echoes it.
//!
//! Verified on the wire against KiCad 10.99 (see docs/kicad-api-compatibility.md).

use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const SP_PROTOCOL_REQ0: u16 = 0x30;
const SP_PROTOCOL_REP0: u16 = 0x31;
const IPC_MSG_TYPE: u8 = 0x01;
const REQUEST_ID_FLAG: u32 = 0x8000_0000;

/// Upper bound for one reply; protects against a corrupt length prefix.
pub const MAX_MESSAGE_BYTES: u64 = 64 * 1024 * 1024;

#[cfg(windows)]
pub type PlatformStream = tokio::net::windows::named_pipe::NamedPipeClient;
#[cfg(unix)]
pub type PlatformStream = tokio::net::UnixStream;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("could not connect to KiCad API endpoint {endpoint}: {source}")]
    Connect { endpoint: String, source: io::Error },
    #[error("timed out connecting to KiCad API endpoint {0}")]
    ConnectTimeout(String),
    #[error("I/O error on KiCad API connection: {0}")]
    Io(#[from] io::Error),
    #[error("KiCad API endpoint did not complete the NNG handshake: {0}")]
    Handshake(String),
    #[error("malformed NNG frame: {0}")]
    Frame(String),
}

/// The OS-level endpoint name for a KiCad socket path.
pub fn endpoint_name(socket_path: &Path) -> String {
    if cfg!(windows) {
        format!(r"\\.\pipe\{}", socket_path.display())
    } else {
        socket_path.display().to_string()
    }
}

/// Opens the platform stream, retrying while a Windows pipe has no free instance.
pub async fn open(socket_path: &Path, timeout: Duration) -> Result<PlatformStream, TransportError> {
    let endpoint = endpoint_name(socket_path);
    let attempt = async {
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ClientOptions;
            const ERROR_PIPE_BUSY: i32 = 231;
            loop {
                match ClientOptions::new().open(&endpoint) {
                    Ok(pipe) => return Ok(pipe),
                    Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                        tokio::time::sleep(Duration::from_millis(50)).await
                    }
                    Err(source) => {
                        return Err(TransportError::Connect {
                            endpoint: endpoint.clone(),
                            source,
                        })
                    }
                }
            }
        }
        #[cfg(unix)]
        {
            tokio::net::UnixStream::connect(socket_path)
                .await
                .map_err(|source| TransportError::Connect {
                    endpoint: endpoint.clone(),
                    source,
                })
        }
    };
    tokio::time::timeout(timeout, attempt)
        .await
        .map_err(|_| TransportError::ConnectTimeout(endpoint.clone()))?
}

/// A connected REQ0 socket with one request in flight at a time.
pub struct ReqSocket<S> {
    stream: S,
    next_id: u32,
}

impl<S: AsyncRead + AsyncWrite + Unpin> ReqSocket<S> {
    /// Performs the SP handshake on an already-open stream.
    pub async fn handshake(mut stream: S) -> Result<Self, TransportError> {
        stream.write_all(&sp_header(SP_PROTOCOL_REQ0)).await?;
        stream.flush().await?;
        let mut peer = [0u8; 8];
        stream.read_exact(&mut peer).await?;
        if peer[..4] != [0, b'S', b'P', 0] {
            return Err(TransportError::Handshake(format!("unexpected header {peer:02x?}")));
        }
        let protocol = u16::from_be_bytes([peer[4], peer[5]]);
        if protocol != SP_PROTOCOL_REP0 {
            return Err(TransportError::Handshake(format!(
                "peer protocol 0x{protocol:04x}, expected REP0"
            )));
        }
        Ok(Self { stream, next_id: 1 })
    }

    /// Sends one request body and returns the matching reply body (request ID stripped).
    pub async fn request(&mut self, body: &[u8]) -> Result<Vec<u8>, TransportError> {
        let id = REQUEST_ID_FLAG | self.next_id;
        self.next_id = if self.next_id >= 0x7FFF_FFFF {
            1
        } else {
            self.next_id + 1
        };

        let mut frame = Vec::with_capacity(9 + 4 + body.len());
        frame.push(IPC_MSG_TYPE);
        frame.extend_from_slice(&(4 + body.len() as u64).to_be_bytes());
        frame.extend_from_slice(&id.to_be_bytes());
        frame.extend_from_slice(body);
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;

        loop {
            let reply = self.read_message().await?;
            if reply.len() < 4 {
                return Err(TransportError::Frame("reply shorter than its request ID".into()));
            }
            let reply_id = u32::from_be_bytes([reply[0], reply[1], reply[2], reply[3]]);
            // A reply to an earlier, abandoned request is discarded, as an NNG REQ socket would.
            if reply_id == id {
                return Ok(reply[4..].to_vec());
            }
        }
    }

    async fn read_message(&mut self) -> Result<Vec<u8>, TransportError> {
        let mut header = [0u8; 9];
        self.stream.read_exact(&mut header).await?;
        if header[0] != IPC_MSG_TYPE {
            return Err(TransportError::Frame(format!("message type 0x{:02x}", header[0])));
        }
        let len = u64::from_be_bytes(header[1..9].try_into().expect("8 bytes"));
        if len > MAX_MESSAGE_BYTES {
            return Err(TransportError::Frame(format!("message of {len} bytes exceeds limit")));
        }
        let mut body = vec![0u8; len as usize];
        self.stream.read_exact(&mut body).await?;
        Ok(body)
    }
}

fn sp_header(protocol: u16) -> [u8; 8] {
    let p = protocol.to_be_bytes();
    [0, b'S', b'P', 0, p[0], p[1], 0, 0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    /// Minimal REP0 peer: answers the handshake, then echoes each request with a prefix.
    async fn fake_rep(mut io: tokio::io::DuplexStream) {
        let mut hdr = [0u8; 8];
        io.read_exact(&mut hdr).await.unwrap();
        assert_eq!(hdr, sp_header(SP_PROTOCOL_REQ0));
        io.write_all(&sp_header(SP_PROTOCOL_REP0)).await.unwrap();
        loop {
            let mut h = [0u8; 9];
            if io.read_exact(&mut h).await.is_err() {
                return;
            }
            let len = u64::from_be_bytes(h[1..9].try_into().unwrap()) as usize;
            let mut body = vec![0u8; len];
            io.read_exact(&mut body).await.unwrap();
            let (id, payload) = body.split_at(4);
            let mut reply = id.to_vec();
            reply.extend_from_slice(b"re:");
            reply.extend_from_slice(payload);
            let mut out = vec![IPC_MSG_TYPE];
            out.extend_from_slice(&(reply.len() as u64).to_be_bytes());
            out.extend_from_slice(&reply);
            io.write_all(&out).await.unwrap();
        }
    }

    #[tokio::test]
    async fn round_trip_with_fake_rep_peer() {
        let (client, server) = duplex(1024);
        tokio::spawn(fake_rep(server));
        let mut sock = ReqSocket::handshake(client).await.unwrap();
        assert_eq!(sock.request(b"one").await.unwrap(), b"re:one");
        assert_eq!(sock.request(b"two").await.unwrap(), b"re:two");
    }

    #[tokio::test]
    async fn rejects_non_rep_peer() {
        let (client, mut server) = duplex(64);
        tokio::spawn(async move {
            let mut hdr = [0u8; 8];
            server.read_exact(&mut hdr).await.unwrap();
            server.write_all(&sp_header(0x10)).await.unwrap(); // PAIR0, not REP0
        });
        assert!(matches!(
            ReqSocket::handshake(client).await,
            Err(TransportError::Handshake(_))
        ));
    }

    #[test]
    fn windows_endpoint_name_prefixes_pipe_namespace() {
        if cfg!(windows) {
            assert_eq!(endpoint_name(Path::new(r"C:\t\api.sock")), r"\\.\pipe\C:\t\api.sock");
        }
    }
}
