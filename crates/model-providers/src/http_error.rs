//! HTTP retry metadata carried through the core's existing string error boundary.

use std::time::{Duration, SystemTime};

use orca_harness_core::ModelError;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "provider HTTP error: ";
const AUTH_PREFIX: &str = "HTTP 401: ";

#[derive(Serialize, Deserialize)]
struct HttpFailure {
    // A stream error may carry a provider code without an HTTP status.
    status: Option<u16>,
    code: Option<String>,
    retry_after: Option<Duration>,
    message: String,
}

pub(crate) async fn check_response(
    response: reqwest::Response,
) -> Result<reqwest::Response, ModelError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.text().await.unwrap_or_default();
    Err(request_error(
        status.as_u16(),
        retry_after.as_deref(),
        &body,
    ))
}

/// Turn a transport failure into a `ModelError` without losing its cause.
///
/// `reqwest::Error`'s `Display` is only the outermost layer, typically
/// "error sending request for url (...)". The part an operator needs is
/// underneath it: "client error (Connect)" and then the io or TLS error such
/// as "invalid peer certificate: UnknownIssuer" or "Connection refused".
/// `to_string()` drops all of that, and `ModelError::Request` carries a plain
/// string, so the chain has to be flattened here. Layers are joined with
/// " <- ", outermost first.
pub fn transport_error(error: &(dyn std::error::Error + 'static)) -> ModelError {
    ModelError::Request(describe(error))
}

pub(crate) fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        let text = inner.to_string();
        if !text.is_empty() && !out.ends_with(&text) {
            out.push_str(" <- ");
            out.push_str(&text);
        }
        source = inner.source();
    }
    out
}

/// Preserve status, provider code and Retry-After without extending ModelError.
pub fn request_error(status: u16, retry_after: Option<&str>, body: &str) -> ModelError {
    let payload: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    encode_failure(failure(Some(status), retry_after, &payload["error"], body))
}

fn failure(
    status: Option<u16>,
    retry_after: Option<&str>,
    error: &serde_json::Value,
    body: &str,
) -> HttpFailure {
    let code = error["code"].as_str().or_else(|| error["type"].as_str());
    HttpFailure {
        status,
        code: code.map(str::to_owned),
        retry_after: retry_after.and_then(|value| {
            value
                .trim()
                .parse::<u64>()
                .ok()
                .map(Duration::from_secs)
                .or_else(|| {
                    httpdate::parse_http_date(value)
                        .ok()
                        .map(|date| date.duration_since(SystemTime::now()).unwrap_or_default())
                })
        }),
        message: body
            .chars()
            .take(4096)
            .map(|ch| {
                if ch.is_control() && !ch.is_whitespace() {
                    ' '
                } else {
                    ch
                }
            })
            .collect(),
    }
}

/// SSE failures share HTTP classification, even when no HTTP status is supplied.
pub(crate) fn stream_error(error: &serde_json::Value) -> ModelError {
    let status = error["status"]
        .as_u64()
        .or_else(|| error["code"].as_u64())
        .and_then(|status| u16::try_from(status).ok());
    encode_failure(failure(status, None, error, &error.to_string()))
}

fn encode_failure(failure: HttpFailure) -> ModelError {
    if failure.status == Some(401) {
        return ModelError::Authentication(format!("HTTP 401: {}", failure.message));
    }
    ModelError::Request(format!(
        "{PREFIX}{}",
        serde_json::to_string(&failure).unwrap()
    ))
}

/// A provider failure rendered for a human instead of a JSON dump.
///
/// The wire payload is doubly encoded (`HttpFailure.message` is the raw body,
/// which is itself JSON) and reaches the UI wrapped in `HarnessError`'s
/// Display, so nothing downstream can read it without this.
#[derive(Debug, PartialEq, Eq)]
pub struct Explained {
    /// One-line headline, e.g. "HTTP 404 · no endpoint available".
    pub headline: String,
    /// The provider's own message, cleaned of nesting.
    pub detail: String,
    /// What the operator can do about it, when we know.
    pub hint: Option<String>,
}

/// Parse a run failure string into something worth showing a person.
///
/// Returns `None` for anything that is not an encoded provider failure —
/// cancellations and plain messages must render unchanged.
pub fn explain(error: &str) -> Option<Explained> {
    // `encode_failure` diverts 401 into `ModelError::Authentication`, which
    // carries the raw body rather than the encoded failure, so that shape has
    // to be recognised separately or the most common misconfiguration of all
    // would still reach the screen as JSON.
    let failure = match error.find(PREFIX) {
        Some(start) => {
            serde_json::from_str::<HttpFailure>(error[start + PREFIX.len()..].trim()).ok()?
        }
        None => {
            let start = error.find(AUTH_PREFIX)? + AUTH_PREFIX.len();
            HttpFailure {
                status: Some(401),
                code: None,
                retry_after: None,
                message: error[start..].trim().to_owned(),
            }
        }
    };

    // The body carried in `message` is itself a JSON error envelope, and some
    // providers (OpenRouter) nest a second one inside its `message` field.
    let mut body: serde_json::Value =
        serde_json::from_str(&failure.message).unwrap_or(serde_json::Value::Null);
    let mut detail = String::new();
    let mut configure_url = None;
    for _ in 0..3 {
        let error = &body["error"];
        if let Some(url) = error["metadata"]["ineligibility_reasons"]
            .as_array()
            .and_then(|reasons| reasons.iter().find_map(|r| r["configure_url"].as_str()))
        {
            configure_url = Some(url.to_owned());
        }
        let Some(message) = error["message"].as_str() else {
            break;
        };
        detail = message.to_owned();
        match serde_json::from_str::<serde_json::Value>(message) {
            Ok(nested) if nested["error"].is_object() => body = nested,
            _ => break,
        }
    }
    // Falling back to the raw body is only an improvement when the body is not
    // itself JSON; otherwise the dump this function exists to prevent returns.
    if detail.trim().is_empty() {
        detail = match serde_json::from_str::<serde_json::Value>(&failure.message) {
            Ok(serde_json::Value::Object(_) | serde_json::Value::Array(_)) => {
                "the provider gave no explanation".to_owned()
            }
            _ => failure.message.clone(),
        };
    }
    detail = collapse(&detail);

    let status = failure.status;
    let summary = match status {
        Some(400) => "malformed request",
        Some(401) => "not authenticated",
        Some(402) => "out of credits",
        Some(403) => "blocked by policy",
        Some(404) => "no endpoint available",
        Some(408) | Some(429) => "rate limited",
        Some(413) => "request too large",
        Some(500..=599) => "provider outage",
        _ => "request rejected",
    };
    let headline = match status {
        Some(status) => format!("HTTP {status} · {summary}"),
        None => format!("provider error · {summary}"),
    };

    let hint = configure_url
        .map(|url| format!("adjust your provider settings at {url}, or pick another model"))
        .or_else(|| {
            Some(
                match status {
                    Some(401) => "check the API key for this provider (/model to switch)",
                    Some(402) => "top up the provider account, or switch to another model",
                    Some(403) => "the account or region is not allowed to use this model",
                    Some(404) => "no provider can serve this model right now — try another one",
                    Some(408) | Some(429) => {
                        if let Some(delay) = failure.retry_after {
                            return Some(format!("retry in {}s", delay.as_secs().max(1)));
                        }
                        "wait a moment and retry, or switch models"
                    }
                    Some(413) => "shorten the conversation (/clear) or send fewer files",
                    Some(500..=599) => "the provider is failing — retry, or switch models",
                    _ => return None,
                }
                .to_string(),
            )
        });

    Some(Explained {
        headline,
        detail,
        hint,
    })
}

/// Provider messages arrive with escaped newlines and runs of whitespace.
fn collapse(text: &str) -> String {
    let flattened: String = text
        .chars()
        .map(|ch| if ch.is_whitespace() { ' ' } else { ch })
        .collect();
    flattened.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Provider policy for RetryModel: None is permanent; Some is a minimum wait.
/// Unstructured transport failures retain the existing retry behavior.
pub fn retry_delay(error: &ModelError) -> Option<Duration> {
    match error {
        ModelError::Request(message) => {
            let failure = message
                .strip_prefix(PREFIX)
                .and_then(|json| serde_json::from_str::<HttpFailure>(json).ok());
            match failure {
                Some(failure) => {
                    if matches!(
                        failure.code.as_deref(),
                        Some(
                            "insufficient_quota"
                                | "usage_limit_reached"
                                | "billing_hard_limit_reached"
                        )
                    ) {
                        return None;
                    }
                    let delay = failure.retry_after.unwrap_or_default();
                    // Unknown stream statuses retain transport retry behavior;
                    // explicit statuses and permanent quota codes narrow it.
                    (matches!(failure.status, None | Some(408 | 429 | 500..=599))
                        && std::time::Instant::now().checked_add(delay).is_some())
                    .then_some(delay)
                }
                None => Some(Duration::ZERO),
            }
        }
        ModelError::OutputLimit { .. } | ModelError::IncompleteResponse { .. } => {
            Some(Duration::ZERO)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_unwraps_a_nested_openrouter_data_policy_rejection() {
        let inner = serde_json::json!({
            "error": {
                "message": "No endpoints found matching your data policy (Paid model training).",
                "code": 404,
                "metadata": {
                    "ineligibility_reasons": [{
                        "reason": "paid-model-training-violation",
                        "endpoint_count": 1,
                        "configure_url": "https://openrouter.ai/settings/privacy"
                    }]
                }
            }
        });
        let body = serde_json::json!({ "error": { "message": inner.to_string(), "code": 404 } });
        let ModelError::Request(wire) = request_error(404, None, &body.to_string()) else {
            panic!("expected Request");
        };
        // The UI sees the HarnessError Display wrapping, not the bare error.
        let explained = explain(&format!("model error: request failed: {wire}")).unwrap();
        assert_eq!(explained.headline, "HTTP 404 · no endpoint available");
        assert_eq!(
            explained.detail,
            "No endpoints found matching your data policy (Paid model training)."
        );
        assert_eq!(
            explained.hint.unwrap(),
            "adjust your provider settings at https://openrouter.ai/settings/privacy, or pick another model"
        );
    }

    #[test]
    fn explain_uses_status_hints_and_retry_after() {
        let body = serde_json::json!({"error": {"message": "slow down"}}).to_string();
        let ModelError::Request(wire) = request_error(429, Some("12"), &body) else {
            panic!("expected Request");
        };
        let explained = explain(&wire).unwrap();
        assert_eq!(explained.headline, "HTTP 429 · rate limited");
        assert_eq!(explained.detail, "slow down");
        assert_eq!(explained.hint.unwrap(), "retry in 12s");
    }

    #[test]
    fn explain_reads_the_authentication_variant_401_is_diverted_into() {
        let body = serde_json::json!({"error": {"message": "No auth credentials found"}});
        let ModelError::Authentication(wire) = request_error(401, None, &body.to_string()) else {
            panic!("expected Authentication");
        };
        let explained = explain(&format!("model error: authentication failed: {wire}")).unwrap();
        assert_eq!(explained.headline, "HTTP 401 · not authenticated");
        assert_eq!(explained.detail, "No auth credentials found");
        assert_eq!(
            explained.hint.unwrap(),
            "check the API key for this provider (/model to switch)"
        );
    }

    #[test]
    fn explain_never_falls_back_to_a_json_body_it_could_not_read() {
        let body = serde_json::json!({"error": {"code": 500}}).to_string();
        let ModelError::Request(wire) = request_error(500, None, &body) else {
            panic!("expected Request");
        };
        let explained = explain(&wire).unwrap();
        assert_eq!(explained.detail, "the provider gave no explanation");
    }

    #[test]
    fn explain_leaves_plain_messages_alone() {
        for text in ["cancelled", "model endpoint unavailable", ""] {
            assert!(explain(text).is_none());
        }
    }

    #[test]
    fn explain_falls_back_to_the_raw_body_when_it_is_not_json() {
        let ModelError::Request(wire) = request_error(500, None, "upstream exploded\n\n") else {
            panic!("expected Request");
        };
        let explained = explain(&wire).unwrap();
        assert_eq!(explained.headline, "HTTP 500 · provider outage");
        assert_eq!(explained.detail, "upstream exploded");
    }

    #[test]
    fn http_retry_classification_and_timing() {
        for status in [400, 401, 403, 404, 422] {
            assert_eq!(
                retry_delay(&request_error(status, None, "bad request")),
                None
            );
        }
        for status in [408, 429, 500, 502, 503] {
            assert_eq!(
                retry_delay(&request_error(status, Some("7"), "busy")),
                Some(Duration::from_secs(7))
            );
        }
        for code in [
            "insufficient_quota",
            "usage_limit_reached",
            "billing_hard_limit_reached",
        ] {
            let body = serde_json::json!({"error": {"code": code}}).to_string();
            assert_eq!(retry_delay(&request_error(429, None, &body)), None);
        }
        assert_eq!(
            retry_delay(&request_error(429, Some("invalid"), "busy")),
            Some(Duration::ZERO)
        );
        assert_eq!(
            retry_delay(&request_error(
                429,
                Some("Sun, 06 Nov 1994 08:49:37 GMT"),
                "busy"
            )),
            Some(Duration::ZERO)
        );
        let date = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(60));
        let delay = retry_delay(&request_error(429, Some(&date), "busy")).unwrap();
        assert!(delay > Duration::from_secs(58) && delay <= Duration::from_secs(60));
    }
}

#[cfg(test)]
mod chain_tests {
    use std::error::Error;
    use std::fmt;

    #[derive(Debug)]
    struct Layer {
        message: &'static str,
        inner: Option<Box<Layer>>,
    }

    impl fmt::Display for Layer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.message)
        }
    }

    impl Error for Layer {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.inner
                .as_deref()
                .map(|inner| inner as &(dyn Error + 'static))
        }
    }

    #[test]
    fn describe_keeps_every_layer_outermost_first() {
        let error = Layer {
            message: "error sending request for url (https://api.example.test/v1/chat/completions)",
            inner: Some(Box::new(Layer {
                message: "client error (Connect)",
                inner: Some(Box::new(Layer {
                    message: "invalid peer certificate: UnknownIssuer",
                    inner: None,
                })),
            })),
        };
        assert_eq!(
            super::describe(&error),
            "error sending request for url (https://api.example.test/v1/chat/completions) <- client error (Connect) <- invalid peer certificate: UnknownIssuer"
        );
    }

    #[test]
    fn describe_leaves_a_single_layer_alone() {
        let error = Layer {
            message: "timed out",
            inner: None,
        };
        assert_eq!(super::describe(&error), "timed out");
    }

    #[test]
    fn transport_error_is_a_request_error() {
        let error = Layer {
            message: "boom",
            inner: None,
        };
        match super::transport_error(&error) {
            orca_harness_core::ModelError::Request(text) => assert_eq!(text, "boom"),
            other => panic!("expected Request, got {other:?}"),
        }
    }
}
