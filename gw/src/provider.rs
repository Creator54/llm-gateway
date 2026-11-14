//! The contract every provider adapter implements.

use bytes::Bytes;
use futures_util::{future::BoxFuture, stream::BoxStream};
use http::{HeaderMap, StatusCode};
use std::time::Duration;

/// Client protocol a request arrived on; responses go back in the same protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inbound {
    /// OpenAI `/v1/responses`
    Responses,
    /// OpenAI `/v1/chat/completions`
    Chat,
    /// Anthropic `/v1/messages`
    Messages,
}

/// One client request after routing picked a provider model.
pub struct Request {
    pub inbound: Inbound,
    /// Provider's own model id (without the `provider/` prefix).
    pub model: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub stream: bool,
    /// Stable per conversation; keeps a session on one account.
    pub session: String,
}

#[derive(Debug, Clone)]
pub enum Credential {
    None,
    ApiKey(String),
    OAuth {
        access: String,
        refresh: Option<String>,
        expires_at: Option<u64>,
    },
}

#[derive(Debug, Clone)]
pub struct Account {
    pub id: String,
    pub credential: Credential,
}

/// Response already in the client's protocol, streamed as bytes.
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: BoxStream<'static, Result<Bytes, std::io::Error>>,
}

/// Why a request failed before any reply byte was sent. Decides failover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// 429 or provider rate limit; try another account, then another target.
    RateLimited { retry_after: Option<Duration> },
    /// Credential rejected; refresh or skip the account.
    Auth,
    /// Plan quota used up; cool the account down until `reset_after`.
    Quota { reset_after: Option<Duration> },
    /// Network or 5xx; safe to retry elsewhere.
    Retryable,
    /// The request itself is wrong; return the error to the client.
    Fatal { status: StatusCode, body: Bytes },
}

pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn send<'a>(
        &'a self,
        req: &'a Request,
        account: &'a Account,
    ) -> BoxFuture<'a, Result<Reply, Failure>>;
}
