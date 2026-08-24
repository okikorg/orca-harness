//! Reusable OAuth 2.0 device authorization flow.

use serde_json::Value;
use std::time::Duration;

pub struct DeviceOAuth<'a> {
    pub client_id: &'a str,
    pub initiation_url: &'a str,
    pub polling_url: &'a str,
    pub verification_url: &'a str,
    pub request_timeout: Duration,
    pub max_polls: usize,
}

pub struct DeviceGrant {
    pub authorization_code: String,
    pub code_verifier: String,
}

impl DeviceOAuth<'_> {
    pub async fn authorize(
        &self,
        client: &reqwest::Client,
        progress: impl Fn(String),
    ) -> Result<DeviceGrant, String> {
        let response = client
            .post(self.initiation_url)
            .timeout(self.request_timeout)
            .json(&serde_json::json!({"client_id": self.client_id}))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let value: Value = response.json().await.map_err(|error| error.to_string())?;
        if !status.is_success() {
            return Err(format!("device authorization failed (HTTP {status})"));
        }
        let device_id = field(&value, "device_auth_id")?;
        let user_code = field(&value, "user_code")?;
        let interval = value
            .get("interval")
            .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
            .unwrap_or(5)
            .saturating_add(3);
        progress(format!(
            "sign in at {} · code: {user_code}",
            self.verification_url
        ));
        for _ in 0..self.max_polls {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            let response = client
                .post(self.polling_url)
                .timeout(self.request_timeout)
                .json(&serde_json::json!({"device_auth_id": device_id, "user_code": user_code}))
                .send()
                .await
                .map_err(|error| error.to_string())?;
            if is_pending(response.status().as_u16()) {
                continue;
            }
            let status = response.status();
            let value: Value = response.json().await.map_err(|error| error.to_string())?;
            if !status.is_success() {
                return Err(format!("device login failed (HTTP {status})"));
            }
            return Ok(DeviceGrant {
                authorization_code: field(&value, "authorization_code")?,
                code_verifier: field(&value, "code_verifier")?,
            });
        }
        Err("device authorization timed out".into())
    }
}

fn field(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("OAuth response has no {key}"))
}

fn is_pending(status: u16) -> bool {
    status == 403
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_forbidden_means_authorization_pending() {
        assert!(super::is_pending(403));
        assert!(!super::is_pending(404));
    }
}
