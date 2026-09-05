//! HTTP retry metadata carried through the core's existing string error boundary.

use std::time::{Duration, SystemTime};

use orca_harness_core::ModelError;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "provider HTTP error: ";

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
