// SPDX-License-Identifier: GPL-3.0-or-later
//! KiCad API request/reply client.

use std::path::Path;
use std::time::Duration;

use prost::{Message, Name};
use prost_types::Any;

use crate::proto::kiapi::common::{ApiRequest, ApiRequestHeader, ApiResponse, ApiStatusCode};
use crate::transport::{self, PlatformStream, ReqSocket, TransportError};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("KiCad did not reply to {request} within {timeout:?}")]
    Timeout { request: String, timeout: Duration },
    #[error("KiCad answered {request} with {code:?}: {message}")]
    Status {
        request: String,
        code: ApiStatusCode,
        message: String,
    },
    #[error("KiCad answered {request} with {actual}, expected {expected}")]
    UnexpectedType {
        request: String,
        expected: String,
        actual: String,
    },
    #[error("could not decode reply to {request}: {source}")]
    Decode {
        request: String,
        source: prost::DecodeError,
    },
    #[error("could not encode {request}: {source}")]
    Encode {
        request: String,
        source: prost::EncodeError,
    },
}

impl Error {
    /// True for statuses KiCad documents as transient (busy, still starting up).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Error::Status {
                code: ApiStatusCode::AsBusy | ApiStatusCode::AsNotReady,
                ..
            }
        )
    }

    /// True when the KiCad build has no handler for the request (feature unavailable).
    pub fn is_unhandled(&self) -> bool {
        matches!(
            self,
            Error::Status {
                code: ApiStatusCode::AsUnhandled | ApiStatusCode::AsUnimplemented,
                ..
            }
        )
    }
}

/// One serialized connection to one KiCad instance.
///
/// The instance token is empty until the first reply, then pinned: later requests carry it, so
/// KiCad rejects them with `AS_TOKEN_MISMATCH` if a different instance now owns the endpoint.
pub struct Client {
    socket: ReqSocket<PlatformStream>,
    client_name: String,
    token: String,
    timeout: Duration,
}

impl Client {
    pub async fn connect(socket_path: &Path, client_name: &str, timeout: Duration) -> Result<Self, Error> {
        let stream = transport::open(socket_path, timeout).await?;
        let socket = tokio::time::timeout(timeout, ReqSocket::handshake(stream))
            .await
            .map_err(|_| Error::Timeout {
                request: "NNG handshake".into(),
                timeout,
            })??;
        Ok(Self {
            socket,
            client_name: client_name.to_owned(),
            token: String::new(),
            timeout,
        })
    }

    /// The instance token pinned from KiCad's first reply, if any.
    pub fn instance_token(&self) -> Option<&str> {
        (!self.token.is_empty()).then_some(self.token.as_str())
    }

    /// Pins a previously observed instance token, e.g. when reconnecting to a known session.
    /// KiCad rejects later requests with `AS_TOKEN_MISMATCH` if another instance answers.
    pub fn pin_instance_token(&mut self, token: impl Into<String>) {
        self.token = token.into();
    }

    /// Sends `request` and decodes the reply as `Resp`.
    pub async fn call<Req: Name, Resp: Name + Default>(&mut self, request: &Req) -> Result<Resp, Error> {
        let name = Req::full_name();
        let reply = self.call_any(request).await?;
        let actual = reply.as_ref().map(|any| any.type_url.clone()).unwrap_or_default();
        match reply {
            Some(any) if any.type_url == Resp::type_url() => {
                Resp::decode(any.value.as_slice()).map_err(|source| Error::Decode { request: name, source })
            }
            _ => Err(Error::UnexpectedType {
                request: name,
                expected: Resp::type_url(),
                actual,
            }),
        }
    }

    /// Sends `request` and returns the raw reply payload (`None` if KiCad sent none).
    pub async fn call_any<Req: Name>(&mut self, request: &Req) -> Result<Option<Any>, Error> {
        let name = Req::full_name();
        let message = Any::from_msg(request).map_err(|source| Error::Encode {
            request: name.clone(),
            source,
        })?;
        let envelope = ApiRequest {
            header: Some(ApiRequestHeader {
                kicad_token: self.token.clone(),
                client_name: self.client_name.clone(),
            }),
            message: Some(message),
        };
        let timeout = self.timeout;
        let bytes = tokio::time::timeout(timeout, self.socket.request(&envelope.encode_to_vec()))
            .await
            .map_err(|_| Error::Timeout {
                request: name.clone(),
                timeout,
            })??;
        let response = ApiResponse::decode(bytes.as_slice()).map_err(|source| Error::Decode {
            request: name.clone(),
            source,
        })?;

        if self.token.is_empty() {
            if let Some(header) = &response.header {
                self.token = header.kicad_token.clone();
            }
        }
        let status = response.status.unwrap_or_default();
        let code = ApiStatusCode::try_from(status.status).unwrap_or(ApiStatusCode::AsUnknown);
        if code != ApiStatusCode::AsOk {
            return Err(Error::Status {
                request: name,
                code,
                message: status.error_message,
            });
        }
        Ok(response.message)
    }
}
