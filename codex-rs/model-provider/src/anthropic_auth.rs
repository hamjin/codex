use codex_api::AuthProvider;
use http::HeaderMap;
use http::HeaderValue;

/// Anthropic API key auth provider that attaches `x-api-key` and
/// `anthropic-version` headers to requests.
///
/// Unlike Bearer-based auth that uses `Authorization: Bearer <token>`,
/// Anthropic's Messages API uses `x-api-key: <key>` for authentication.
#[derive(Clone, Default)]
pub struct AnthropicApiKeyAuthProvider {
    pub api_key: Option<String>,
}

impl AnthropicApiKeyAuthProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key: Some(api_key),
        }
    }
}

impl AuthProvider for AnthropicApiKeyAuthProvider {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        if let Some(api_key) = self.api_key.as_ref()
            && let Ok(header) = HeaderValue::from_str(api_key)
        {
            let _ = headers.insert("x-api-key", header);
        }
        if let Ok(header) = HeaderValue::from_str("2023-06-01") {
            let _ = headers.insert("anthropic-version", header);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn anthropic_auth_adds_api_key_and_version_headers() {
        let auth = AnthropicApiKeyAuthProvider::new("sk-ant-test-key".to_string());
        let mut headers = HeaderMap::new();

        auth.add_auth_headers(&mut headers);

        assert_eq!(
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok()),
            Some("sk-ant-test-key")
        );
        assert_eq!(
            headers
                .get("anthropic-version")
                .and_then(|value| value.to_str().ok()),
            Some("2023-06-01")
        );
    }

    #[test]
    fn anthropic_auth_without_key_adds_only_version_header() {
        let auth = AnthropicApiKeyAuthProvider::default();
        let mut headers = HeaderMap::new();

        auth.add_auth_headers(&mut headers);

        assert!(headers.get("x-api-key").is_none());
        assert_eq!(
            headers
                .get("anthropic-version")
                .and_then(|value| value.to_str().ok()),
            Some("2023-06-01")
        );
    }

    #[test]
    fn anthropic_auth_reports_auth_header_telemetry() {
        let auth = AnthropicApiKeyAuthProvider::new("sk-ant-test-key".to_string());

        // auth_header_telemetry only checks for the `Authorization` header.
        // Anthropic uses `x-api-key` so it reports as not attached.
        assert_eq!(
            codex_api::auth_header_telemetry(&auth),
            codex_api::AuthHeaderTelemetry {
                attached: false,
                name: None,
            }
        );
    }
}
