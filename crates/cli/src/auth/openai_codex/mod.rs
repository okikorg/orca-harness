//! OpenAI Codex subscription authentication.

use crate::auth::{oauth_device::DeviceOAuth, store};
use async_trait::async_trait;
use orca_harness_model_providers::openai_codex::{CodexCredential, CodexCredentialSource};
use orca_harness_provider_auth::{
    BearerCredential, CredentialError, CredentialErrorKind, CredentialSource,
};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
const DEVICE_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_LOGIN_URL: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const REFRESH_WINDOW_SECS: u64 = 300;

/// Imports and refreshes the official Codex CLI login. The mutex makes token
/// rotation single-flight; atomic replacement prevents partial auth files.
pub struct CodexCliCredential {
    path: PathBuf,
    client: reqwest::Client,
    refresh_url: String,
    state: Mutex<Option<CodexCredential>>,
}

impl CodexCliCredential {
    pub fn discover() -> Self {
        let owned = orcacode_auth_path().ok();
        let path = if owned.as_ref().is_some_and(|path| path.is_file()) {
            owned.expect("checked")
        } else {
            codex_auth_path()
        };
        Self::new(path, REFRESH_URL.into())
    }

    /// Complete Orcacode's own headless-friendly OpenAI device OAuth flow.
    fn new(path: PathBuf, refresh_url: String) -> Self {
        Self {
            path,
            client: reqwest::Client::new(),
            refresh_url,
            state: Mutex::new(None),
        }
    }

    async fn current(&self, force: bool) -> Result<CodexCredential, CredentialError> {
        let mut state = self.state.lock().await;
        let (root, disk, refresh_token) = read_auth(&self.path)?;
        let current = state.clone().unwrap_or(disk);
        if !force && !needs_refresh(current.bearer.expires_at) {
            *state = Some(current.clone());
            return Ok(current);
        }
        let refresh_token = refresh_token.ok_or_else(|| {
            CredentialError::new(
                CredentialErrorKind::Expired,
                "Codex access token expired and has no refresh token; select openai-codex to sign in again",
            )
        })?;
        let (renewed, rotated_refresh) = self.exchange(&current.account_id, &refresh_token).await?;
        persist_tokens(
            &self.path,
            root,
            &renewed,
            rotated_refresh.as_deref().unwrap_or(&refresh_token),
        )?;
        *state = Some(renewed.clone());
        Ok(renewed)
    }

    async fn recover_unauthorized(
        &self,
        rejected: &str,
    ) -> Result<CodexCredential, CredentialError> {
        let mut state = self.state.lock().await;
        if let Some(current) = state
            .as_ref()
            .filter(|value| value.bearer.access_token != rejected)
        {
            return Ok(current.clone());
        }
        let (root, disk, refresh) = read_auth(&self.path)?;
        if disk.bearer.access_token != rejected {
            *state = Some(disk.clone());
            return Ok(disk);
        }
        let refresh = refresh.ok_or_else(|| {
            CredentialError::new(
                CredentialErrorKind::Expired,
                "Codex login expired; select openai-codex to sign in again",
            )
        })?;
        let (renewed, rotated) = self.exchange(&disk.account_id, &refresh).await?;
        persist_tokens(
            &self.path,
            root,
            &renewed,
            rotated.as_deref().unwrap_or(&refresh),
        )?;
        *state = Some(renewed.clone());
        Ok(renewed)
    }

    async fn exchange(
        &self,
        account_id: &str,
        refresh_token: &str,
    ) -> Result<(CodexCredential, Option<String>), CredentialError> {
        let response = self
            .client
            .post(&self.refresh_url)
            .form(&refresh_form(refresh_token))
            .send()
            .await
            .map_err(|error| {
                CredentialError::new(
                    CredentialErrorKind::Refresh,
                    format!("Codex token refresh failed: {error}"),
                )
            })?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|error| {
            CredentialError::new(
                CredentialErrorKind::Refresh,
                format!("Codex refresh response failed: {error}"),
            )
        })?;
        if !status.is_success() {
            return Err(CredentialError::new(
                CredentialErrorKind::Expired,
                format!(
                    "Codex login could not be refreshed (HTTP {status}); select openai-codex to sign in again"
                ),
            ));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
            CredentialError::new(
                CredentialErrorKind::Malformed,
                "Codex refresh returned malformed credentials",
            )
        })?;
        let access_token = required_token(&value, "access_token")?;
        let refresh = value
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok((
            CodexCredential {
                bearer: BearerCredential {
                    expires_at: jwt_exp(&access_token),
                    access_token,
                },
                account_id: account_id.to_owned(),
            },
            refresh,
        ))
    }

    pub fn status(&self) -> Result<&'static str, CredentialError> {
        let (_, credential, refresh) = read_auth(&self.path)?;
        if needs_refresh(credential.bearer.expires_at) && refresh.is_none() {
            return Err(CredentialError::new(
                CredentialErrorKind::Expired,
                "Codex login expired; select openai-codex to sign in",
            ));
        }
        Ok("signed in with ChatGPT")
    }

    pub fn status_summary(&self) -> &'static str {
        match self.status() {
            Ok(status) => status,
            Err(error) => match error.kind {
                CredentialErrorKind::Missing => "not signed in · select to log in",
                CredentialErrorKind::Expired => "expired · select to log in",
                CredentialErrorKind::Malformed => "invalid login · select to log in",
                CredentialErrorKind::Refresh => "refresh unavailable",
                CredentialErrorKind::Storage => "credential storage unavailable",
            },
        }
    }
}

pub async fn login(progress: impl Fn(String)) -> Result<CodexCredential, CredentialError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| oauth_error(error.to_string()))?;
    let flow = DeviceOAuth {
        client_id: CLIENT_ID,
        initiation_url: DEVICE_CODE_URL,
        polling_url: DEVICE_TOKEN_URL,
        verification_url: DEVICE_LOGIN_URL,
        request_timeout: std::time::Duration::from_secs(15),
        max_polls: 120,
    };
    let grant = flow
        .authorize(&client, &progress)
        .await
        .map_err(oauth_error)?;
    progress("authorization approved · exchanging credentials…".into());
    exchange_device_code(&client, &grant.authorization_code, &grant.code_verifier).await
}

fn oauth_error(message: impl Into<String>) -> CredentialError {
    CredentialError::new(CredentialErrorKind::Refresh, message)
}

fn required_oauth_field(value: &Value, key: &str) -> Result<String, CredentialError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| oauth_error(format!("OAuth response has no {key}")))
}

async fn exchange_device_code(
    client: &reqwest::Client,
    code: &str,
    verifier: &str,
) -> Result<CodexCredential, CredentialError> {
    let response = client
        .post(REFRESH_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", DEVICE_REDIRECT_URI),
        ])
        .send()
        .await
        .map_err(|error| oauth_error(format!("token exchange failed: {error}")))?;
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .map_err(|error| oauth_error(format!("token exchange returned invalid JSON: {error}")))?;
    if !status.is_success() {
        return Err(oauth_error(format!(
            "token exchange failed (HTTP {status})"
        )));
    }
    let access_token = required_oauth_field(&value, "access_token")?;
    let refresh_token = required_oauth_field(&value, "refresh_token")?;
    let account_id = jwt_account_id(&access_token)
        .ok_or_else(|| oauth_error("OpenAI login did not identify a ChatGPT account"))?;
    let credential = CodexCredential {
        bearer: BearerCredential {
            expires_at: jwt_exp(&access_token),
            access_token,
        },
        account_id,
    };
    let path = orcacode_auth_path()?;
    persist_tokens(
        &path,
        json!({"auth_mode":"chatgpt", "tokens":{}}),
        &credential,
        &refresh_token,
    )?;
    Ok(credential)
}

fn refresh_form(refresh_token: &str) -> [(&'static str, &str); 3] {
    [
        ("client_id", CLIENT_ID),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ]
}

#[async_trait]
impl CredentialSource for CodexCliCredential {
    async fn credential(&self) -> Result<BearerCredential, CredentialError> {
        Ok(self.current(false).await?.bearer)
    }
    async fn refresh(&self) -> Result<BearerCredential, CredentialError> {
        Ok(self.current(true).await?.bearer)
    }
}

#[async_trait]
impl CodexCredentialSource for CodexCliCredential {
    async fn codex_credential(&self) -> Result<CodexCredential, CredentialError> {
        self.current(false).await
    }
    async fn refresh_codex(&self, rejected: &str) -> Result<CodexCredential, CredentialError> {
        self.recover_unauthorized(rejected).await
    }
}

fn read_auth(path: &Path) -> Result<(Value, CodexCredential, Option<String>), CredentialError> {
    let bytes = fs::read(path).map_err(|error| {
        CredentialError::new(
            CredentialErrorKind::Missing,
            format!(
                "OpenAI Codex subscription is not signed in; select the provider to log in ({}: {error})",
                path.display()
            ),
        )
    })?;
    let root: Value = serde_json::from_slice(&bytes).map_err(|_| {
        CredentialError::new(
            CredentialErrorKind::Malformed,
            "Codex auth file is malformed; select openai-codex to sign in again",
        )
    })?;
    let tokens = root.get("tokens").unwrap_or(&root);
    let access_token = required_token(tokens, "access_token")?;
    let account_id = tokens
        .get("account_id")
        .or_else(|| root.get("account_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CredentialError::new(
                CredentialErrorKind::Malformed,
                "Codex login has no ChatGPT account; select openai-codex to sign in again",
            )
        })?
        .to_owned();
    let refresh = tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let bearer = BearerCredential {
        expires_at: jwt_exp(&access_token),
        access_token,
    };
    Ok((root, CodexCredential { bearer, account_id }, refresh))
}

fn required_token(value: &Value, key: &str) -> Result<String, CredentialError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            CredentialError::new(
                CredentialErrorKind::Malformed,
                format!("Codex login has no {key}; select openai-codex to sign in again"),
            )
        })
}

fn needs_refresh(expires_at: Option<u64>) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    expires_at.is_some_and(|expiry| expiry <= now.saturating_add(REFRESH_WINDOW_SECS))
}

fn jwt_exp(token: &str) -> Option<u64> {
    let bytes = decode_base64_url(token.split('.').nth(1)?)?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get("exp")?
        .as_u64()
}

fn jwt_account_id(token: &str) -> Option<String> {
    let bytes = decode_base64_url(token.split('.').nth(1)?)?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn decode_base64_url(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let (mut bits, mut count) = (0_u32, 0_u8);
    for byte in input.bytes().filter(|byte| *byte != b'=') {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    Some(out)
}

fn persist_tokens(
    path: &Path,
    mut root: Value,
    credential: &CodexCredential,
    refresh: &str,
) -> Result<(), CredentialError> {
    let tokens = if root.get("tokens").is_some() {
        root.get_mut("tokens").expect("checked")
    } else {
        &mut root
    };
    tokens["access_token"] = json!(credential.bearer.access_token);
    tokens["refresh_token"] = json!(refresh);
    tokens["account_id"] = json!(credential.account_id);
    root["last_refresh"] = json!(utc_timestamp());
    store::write_private_json(path, &root).map_err(|error| {
        CredentialError::new(
            CredentialErrorKind::Storage,
            format!("could not persist refreshed Codex login: {error}"),
        )
    })
}

fn utc_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        day_seconds / 3_600,
        day_seconds % 3_600 / 60,
        day_seconds % 60
    )
}

fn codex_auth_path() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".codex"))
        })
        .unwrap_or_default()
        .join("auth.json")
}

fn orcacode_auth_path() -> Result<PathBuf, CredentialError> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".config"))
        })
        .map(|path| path.join("orcacode/auth/openai-codex.json"))
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            CredentialError::new(
                CredentialErrorKind::Storage,
                "no absolute configuration home is available for OAuth credentials",
            )
        })
}

#[cfg(test)]
#[cfg(test)]
mod tests;
