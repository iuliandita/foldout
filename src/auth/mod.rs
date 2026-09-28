mod service;

use serde::{Deserialize, Serialize};
pub use service::AuthService;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Read,
    Manage,
    Admin,
}

impl Scope {
    pub fn allows(self, required: Self) -> bool {
        matches!(
            (self, required),
            (Self::Admin, _) | (Self::Manage, Self::Read | Self::Manage) | (Self::Read, Self::Read)
        )
    }
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Manage => "manage",
            Self::Admin => "admin",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    Session,
    ApiKey,
}

#[derive(Clone, Debug)]
pub struct Principal {
    pub user_id: String,
    pub scope: Scope,
    pub kind: CredentialKind,
}

pub struct Login {
    pub token: String,
    pub expires_at: i64,
    pub principal: Principal,
}

#[derive(Serialize)]
pub struct CreatedKey {
    pub id: String,
    pub name: String,
    pub scope: Scope,
    pub secret: String,
}

#[derive(Debug, Serialize)]
pub struct KeyInfo {
    pub id: String,
    pub name: String,
    pub scope: Scope,
    pub created_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{0}")]
    Invalid(String),
    #[error("authentication required")]
    Unauthorized,
    #[error("administrator already configured")]
    AlreadyConfigured,
    #[error("too many login attempts")]
    RateLimited,
    #[error("authentication unavailable")]
    Unavailable,
    #[error("authentication database error")]
    Database(#[from] sqlx::Error),
}

#[cfg(test)]
#[path = "auth_test.rs"]
mod tests;
