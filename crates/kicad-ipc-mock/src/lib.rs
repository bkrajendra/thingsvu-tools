// SPDX-License-Identifier: GPL-3.0-or-later
//! A fake KiCad API server for tests: the NNG REP0 side of the ipc transport on a real named pipe
//! (Windows) or Unix socket, answering requests with handlers registered per message type.
//!
//! ```no_run
//! # async fn demo() {
//! use kicad_ipc::proto::kiapi::common::commands::{GetVersion, GetVersionResponse};
//! let mock = kicad_ipc_mock::MockKicad::builder()
//!     .reply::<GetVersion, _>(|_req| Ok(GetVersionResponse::default()))
//!     .start()
//!     .await
//!     .unwrap();
//! // connect a kicad_ipc::Client to mock.socket_path()
//! # }
//! ```

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kicad_ipc::proto::kiapi::common::{ApiRequest, ApiResponse, ApiResponseHeader, ApiResponseStatus, ApiStatusCode};
use prost::{Message, Name};
use prost_types::Any;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;

/// The instance token every mock reply carries.
pub const MOCK_TOKEN: &str = "00000000-0000-4000-8000-00000000c0de";

/// A handler's failure: the status KiCad would report.
pub type Failure = (ApiStatusCode, String);
type Handler = Box<dyn Fn(&Any, &str) -> Result<Option<Any>, Failure> + Send + Sync>;

/// Records every request the mock receives, for assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub type_url: String,
    pub client_name: String,
    pub token: String,
}

pub struct Builder {
    handlers: HashMap<String, Handler>,
}

impl Builder {
    /// Answers requests of type `Req` with the value `f` returns (or the failure status).
    pub fn reply<Req, Resp>(self, f: impl Fn(Req) -> Result<Resp, Failure> + Send + Sync + 'static) -> Self
    where
        Req: Name + Default,
        Resp: Name,
    {
        self.handle::<Req>(move |req, _client| f(req).map(|resp| Some(Any::from_msg(&resp).expect("encode reply"))))
    }

    /// Like `reply`, with access to the requesting client's name and a raw (optional) payload.
    pub fn handle<Req: Name + Default>(
        mut self,
        f: impl Fn(Req, &str) -> Result<Option<Any>, Failure> + Send + Sync + 'static,
    ) -> Self {
        let handler: Handler = Box::new(move |any, client| {
            let req = any
                .to_msg::<Req>()
                .map_err(|e| (ApiStatusCode::AsBadRequest, e.to_string()))?;
            f(req, client)
        });
        self.handlers.insert(Req::type_url(), handler);
        self
    }

    pub async fn start(self) -> io::Result<MockKicad> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let socket_path = std::env::temp_dir().join(format!(
            "thingsvu-mock-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let state = Arc::new(State {
            handlers: self.handlers,
            received: Mutex::new(Vec::new()),
        });
        let task = listen(&socket_path, state.clone())?;
        Ok(MockKicad {
            socket_path,
            state,
            task,
        })
    }
}

struct State {
    handlers: HashMap<String, Handler>,
    received: Mutex<Vec<Received>>,
}

pub struct MockKicad {
    socket_path: PathBuf,
    state: Arc<State>,
    task: JoinHandle<()>,
}

impl MockKicad {
    pub fn builder() -> Builder {
        Builder {
            handlers: HashMap::new(),
        }
    }

    /// The socket path to pass to `kicad_ipc::Client::connect`.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn received(&self) -> Vec<Received> {
        self.state.received.lock().expect("mock state").clone()
    }
}

impl Drop for MockKicad {
    fn drop(&mut self) {
        self.task.abort();
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(windows)]
fn listen(socket_path: &Path, state: Arc<State>) -> io::Result<JoinHandle<()>> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let name = kicad_ipc::transport::endpoint_name(socket_path);
    let mut server = ServerOptions::new().first_pipe_instance(true).create(&name)?;
    Ok(tokio::spawn(async move {
        loop {
            if server.connect().await.is_err() {
                return;
            }
            let connected = server;
            server = match ServerOptions::new().create(&name) {
                Ok(s) => s,
                Err(_) => return,
            };
            tokio::spawn(serve(connected, state.clone()));
        }
    }))
}

#[cfg(unix)]
fn listen(socket_path: &Path, state: Arc<State>) -> io::Result<JoinHandle<()>> {
    let listener = tokio::net::UnixListener::bind(socket_path)?;
    Ok(tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(serve(stream, state.clone()));
        }
    }))
}

async fn serve<S: AsyncRead + AsyncWrite + Unpin>(mut io: S, state: Arc<State>) {
    let mut header = [0u8; 8];
    if io.read_exact(&mut header).await.is_err() {
        return; // bare connect/close, as readiness polling does
    }
    if io.write_all(&[0, b'S', b'P', 0, 0, 0x31, 0, 0]).await.is_err() {
        return;
    }
    loop {
        let mut h = [0u8; 9];
        if io.read_exact(&mut h).await.is_err() {
            return;
        }
        let len = u64::from_be_bytes(h[1..9].try_into().expect("8 bytes")) as usize;
        let mut body = vec![0u8; len];
        if io.read_exact(&mut body).await.is_err() || len < 4 {
            return;
        }
        let (id, payload) = body.split_at(4);
        let reply = answer(&state, payload);
        let mut out = vec![0x01];
        out.extend_from_slice(&((4 + reply.len()) as u64).to_be_bytes());
        out.extend_from_slice(id);
        out.extend_from_slice(&reply);
        if io.write_all(&out).await.is_err() {
            return;
        }
    }
}

fn answer(state: &State, payload: &[u8]) -> Vec<u8> {
    let status = |code: ApiStatusCode, message: String| ApiResponseStatus {
        status: code as i32,
        error_message: message,
    };
    let response = match ApiRequest::decode(payload) {
        Err(e) => ApiResponse {
            header: Some(ApiResponseHeader {
                kicad_token: MOCK_TOKEN.into(),
            }),
            status: Some(status(ApiStatusCode::AsBadRequest, e.to_string())),
            message: None,
        },
        Ok(req) => {
            let header = req.header.unwrap_or_default();
            let any = req.message.unwrap_or_default();
            state.received.lock().expect("mock state").push(Received {
                type_url: any.type_url.clone(),
                client_name: header.client_name.clone(),
                token: header.kicad_token.clone(),
            });
            let result = if !header.kicad_token.is_empty() && header.kicad_token != MOCK_TOKEN {
                Err((
                    ApiStatusCode::AsTokenMismatch,
                    "the provided kicad_token did not match this KiCad instance's token".into(),
                ))
            } else {
                match state.handlers.get(&any.type_url) {
                    Some(handler) => handler(&any, &header.client_name),
                    None => Err((
                        ApiStatusCode::AsUnhandled,
                        format!(
                            "no handler available for request of type {}",
                            any.type_url.trim_start_matches("type.googleapis.com/")
                        ),
                    )),
                }
            };
            match result {
                Ok(message) => ApiResponse {
                    header: Some(ApiResponseHeader {
                        kicad_token: MOCK_TOKEN.into(),
                    }),
                    status: Some(status(ApiStatusCode::AsOk, String::new())),
                    message,
                },
                Err((code, message)) => ApiResponse {
                    header: Some(ApiResponseHeader {
                        kicad_token: MOCK_TOKEN.into(),
                    }),
                    status: Some(status(code, message)),
                    message: None,
                },
            }
        }
    };
    response.encode_to_vec()
}
