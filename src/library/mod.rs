pub mod coverage;
pub mod metadata;
pub mod roots;
pub mod scan;

#[cfg(test)]
mod scan_test;

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LibraryFormat {
    Cbz,
    Cbr,
    Pdf,
}

impl LibraryFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cbz => "cbz",
            Self::Cbr => "cbr",
            Self::Pdf => "pdf",
        }
    }
}
impl FromStr for LibraryFormat {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "cbz" => Ok(Self::Cbz),
            "cbr" => Ok(Self::Cbr),
            "pdf" => Ok(Self::Pdf),
            _ => Err(format!("unsupported library format: {value}")),
        }
    }
}
impl fmt::Display for LibraryFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NewLibraryFile {
    pub path: String,
    pub format: LibraryFormat,
    pub signature: String,
    pub size_bytes: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LibraryFile {
    pub id: String,
    pub path: String,
    pub format: LibraryFormat,
    pub signature: String,
    pub size_bytes: i64,
}

#[cfg(test)]
mod coverage_test;
