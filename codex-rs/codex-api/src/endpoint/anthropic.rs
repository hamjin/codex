use crate::anthropic_types::AnthropicMessagesRequest;
use crate::auth::SharedAuthProvider;
use crate::common::ResponseStream;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::sse::anthropic::spawn_anthropic_stream;
use codex_client::HttpTransport;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use tracing::instrument;

/// Client for the Anthropic Messages API (`POST /v1/messages`).
///
/// Reuses `EndpointSession` for transport, auth, and retry, and delegates
/// SSE parsing to `spawn_anthropic_stream` which maps Anthropic events into
/// the shared `ResponseEvent` enum.
pub struct AnthropicClient<T: HttpTransport> {
    session: EndpointSession<T>,
}

impl<T: HttpTransport> AnthropicClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
        }
    }

    fn path() -> &'static str {
        "messages"
    }

    #[instrument(
        name = "anthropic.stream_request",
        level = "info",
        skip_all,
        fields(
            transport = "anthropic_http",
            http.method = "POST",
            api.path = "messages"
        )
    )]
    pub async fn stream_request(
        &self,
        request: AnthropicMessagesRequest,
    ) -> Result<ResponseStream, ApiError> {
        let body = serde_json::to_value(&request)
            .map_err(|e| ApiError::Stream(format!("failed to encode anthropic request: {e}")))?;

        let headers = HeaderMap::new();

        let stream_response = self
            .session
            .stream_with(Method::POST, Self::path(), headers, Some(body), |req| {
                req.headers.insert(
                    http::header::ACCEPT,
                    HeaderValue::from_static("text/event-stream"),
                );
            })
            .await?;

        Ok(spawn_anthropic_stream(
            stream_response,
            self.session.provider().stream_idle_timeout,
        ))
    }
}
