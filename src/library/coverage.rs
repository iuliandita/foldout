use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageEvidence {
    UserConfirmed,
    TrustedMetadata,
    Filename,
}

impl CoverageEvidence {
    pub fn is_explicit(&self) -> bool {
        matches!(self, Self::UserConfirmed | Self::TrustedMetadata)
    }
}
