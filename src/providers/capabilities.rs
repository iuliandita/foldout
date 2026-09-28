use super::{
    ContentType, MetadataCandidate, MetadataProvider, ProviderConfig, ProviderError, http::Http,
    query_valid,
};
use serde::Serialize;

pub struct GetComics {
    http: Http,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DdlCapability {
    pub source: &'static str,
    pub source_kind: &'static str,
    pub page_reachable: bool,
    pub automated_search: bool,
    pub automated_download: bool,
}

impl GetComics {
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            http: Http::new(config, false)?,
        })
    }

    /// Reachability only. Does not promise a stable API, resolve mirrors, submit
    /// CAPTCHA responses, execute scripts, or follow provider links.
    pub async fn capability(&self) -> Result<DdlCapability, ProviderError> {
        let response = self
            .http
            .execute(
                self.http
                    .get("")?
                    .header(reqwest::header::ACCEPT, "text/html"),
            )
            .await?;
        let html = std::str::from_utf8(&response.body)
            .map_err(|_| ProviderError::InvalidResponse)?
            .to_ascii_lowercase();
        let challenge_form = html.contains("id=\"challenge-form\"")
            || html.contains("id='challenge-form'")
            || html.contains("cf-turnstile-response")
            || html.contains("g-recaptcha-response")
            || html.contains("h-captcha-response");
        if response.challenge
            || challenge_form
            || (html.contains("_cf_chl_opt") && html.contains("/challenge-platform/"))
        {
            return Err(ProviderError::ChallengeRequired);
        }
        if !response.status.is_success() {
            return Err(ProviderError::Unavailable);
        }
        if !response
            .content_type
            .to_ascii_lowercase()
            .starts_with("text/html")
            || !html.contains("<html")
            || !html.contains("getcomics")
        {
            return Err(ProviderError::InvalidResponse);
        }
        Ok(DdlCapability {
            source: "getcomics",
            source_kind: "direct_download",
            page_reachable: true,
            automated_search: false,
            automated_download: false,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MagazineCapability {
    pub local_manual: bool,
    pub issn_identity: bool,
    pub external_metadata_search: bool,
    pub universal_issue_catalog: bool,
}

pub fn magazine_capability() -> MagazineCapability {
    MagazineCapability {
        local_manual: true,
        issn_identity: true,
        external_metadata_search: false,
        universal_issue_catalog: false,
    }
}

/// Caller supplies the locally maintained identity. ISSN identifies a serial,
/// not its issues; this function does not contact or query the ISSN registry.
pub fn magazine_identity(
    local_id: &str,
    title: &str,
    issn: Option<&str>,
) -> Result<MetadataCandidate, ProviderError> {
    query_valid(local_id)?;
    query_valid(title)?;
    let (provider, external_id) = match issn {
        Some(value) => (MetadataProvider::Issn, normalize_issn(value)?),
        None => (MetadataProvider::LocalManual, local_id.trim().to_owned()),
    };
    Ok(MetadataCandidate {
        provider,
        external_id,
        title: title.trim().to_owned(),
        content_type: ContentType::Magazine,
        date: None,
    })
}

fn normalize_issn(value: &str) -> Result<String, ProviderError> {
    let value = value.trim().to_ascii_uppercase();
    if !value.is_ascii() {
        return Err(ProviderError::InvalidQuery);
    }
    let digits = if value.len() == 9 && value.as_bytes()[4] == b'-' {
        format!("{}{}", &value[..4], &value[5..])
    } else {
        value
    };
    let b = digits.as_bytes();
    if b.len() != 8
        || !b[..7].iter().all(u8::is_ascii_digit)
        || !(b[7].is_ascii_digit() || b[7] == b'X')
    {
        return Err(ProviderError::InvalidQuery);
    }
    let sum: u32 = b[..7]
        .iter()
        .enumerate()
        .map(|(i, d)| u32::from(d - b'0') * (8 - i as u32))
        .sum();
    let check = if b[7] == b'X' {
        10
    } else {
        u32::from(b[7] - b'0')
    };
    if !(sum + check).is_multiple_of(11) {
        return Err(ProviderError::InvalidQuery);
    }
    Ok(format!("{}-{}", &digits[..4], &digits[4..]))
}
