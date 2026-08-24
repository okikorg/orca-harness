//! Provider-neutral credentials. Login flows and persistence belong to hosts;
//! protocol adapters consume this narrow boundary.

use async_trait::async_trait;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BearerCredential {
    pub access_token: String,
    pub expires_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialErrorKind {
    Missing,
    Malformed,
    Expired,
    Refresh,
    Storage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialError {
    pub kind: CredentialErrorKind,
    pub message: String,
}

impl CredentialError {
    pub fn new(kind: CredentialErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[async_trait]
pub trait CredentialSource: Send + Sync {
    async fn credential(&self) -> Result<BearerCredential, CredentialError>;
    async fn refresh(&self) -> Result<BearerCredential, CredentialError>;
}

#[derive(Clone)]
pub struct StaticCredential(pub BearerCredential);

#[async_trait]
impl CredentialSource for StaticCredential {
    async fn credential(&self) -> Result<BearerCredential, CredentialError> {
        Ok(self.0.clone())
    }

    async fn refresh(&self) -> Result<BearerCredential, CredentialError> {
        Ok(self.0.clone())
    }
}
