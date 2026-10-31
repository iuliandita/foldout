use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use sqlx::Row;

use super::{metadata::EmbeddedMetadata, roots::Library};

#[derive(Clone, Debug, Serialize)]
pub struct IdentityHint {
    pub kind: String,
    pub raw_value: String,
    pub value: Option<String>,
    pub status: String,
    pub source_field: String,
    pub container_path: String,
    pub extracted_at: i64,
}

impl EmbeddedMetadata {
    pub(crate) fn identity_hints(
        &self,
        container_path: &str,
        extracted_at: i64,
    ) -> Vec<IdentityHint> {
        let mut hints = Vec::new();
        for (field, content) in &self.identity_fields {
            for raw in content.split_whitespace() {
                let identified = match field.as_str() {
                    "ISBN" | "ISBN10" | "ISBN13" => Some(("isbn", book_number(raw, true))),
                    "GTIN" | "GTIN13" | "EAN" | "EAN13" => Some(("gtin", book_number(raw, false))),
                    "ComicVineID" => Some(("comicvine", comicvine(raw))),
                    "MangaDexID" => Some(("mangadex", mangadex(raw))),
                    "MangaUpdatesID" => Some(("mangaupdates", positive_number(raw))),
                    "Web" | "URL" => url_hint(raw),
                    _ => None,
                };
                if let Some((kind, value)) = identified {
                    hints.push(IdentityHint {
                        kind: kind.into(),
                        raw_value: raw.into(),
                        status: if value.is_some() {
                            "usable"
                        } else {
                            "needs_review"
                        }
                        .into(),
                        value,
                        source_field: field.clone(),
                        container_path: container_path.into(),
                        extracted_at,
                    });
                }
            }
        }
        let mut values: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for hint in &hints {
            if let Some(value) = &hint.value {
                values
                    .entry(hint.kind.clone())
                    .or_default()
                    .insert(value.clone());
            }
        }
        for hint in &mut hints {
            if values.get(hint.kind.as_str()).is_some_and(|v| v.len() > 1) {
                hint.status = "needs_review".into();
            }
        }
        hints
    }
}

fn positive_number(raw: &str) -> Option<String> {
    if raw.len() > 20 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value = raw.parse::<u64>().ok()?;
    (value > 0).then(|| value.to_string())
}
fn comicvine(raw: &str) -> Option<String> {
    let id = raw.strip_prefix("4050-")?;
    Some(format!("4050-{}", positive_number(id)?))
}
fn mangadex(raw: &str) -> Option<String> {
    let id = uuid::Uuid::parse_str(raw).ok()?;
    (raw.len() == 36 && !id.is_nil()).then(|| id.hyphenated().to_string())
}
fn book_number(raw: &str, isbn: bool) -> Option<String> {
    if raw.len() > 32
        || !raw
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || (isbn && b == b'X'))
    {
        return None;
    }
    let digits: String = raw.chars().filter(|c| *c != '-').collect();
    let bytes = digits.as_bytes();
    let valid = if isbn && bytes.len() == 10 {
        bytes
            .iter()
            .enumerate()
            .try_fold(0u32, |sum, (i, b)| {
                let n = if i == 9 && *b == b'X' {
                    10
                } else if b.is_ascii_digit() {
                    u32::from(*b - b'0')
                } else {
                    return None;
                };
                Some(sum + (10 - i as u32) * n)
            })
            .is_some_and(|sum| sum % 11 == 0)
    } else if bytes.len() == 13
        && bytes.iter().all(u8::is_ascii_digit)
        && (!isbn || digits.starts_with("978") || digits.starts_with("979"))
    {
        bytes
            .iter()
            .enumerate()
            .map(|(i, b)| u32::from(*b - b'0') * if i % 2 == 0 { 1 } else { 3 })
            .sum::<u32>()
            % 10
            == 0
    } else {
        false
    };
    valid.then_some(digits)
}
fn url_hint(raw: &str) -> Option<(&'static str, Option<String>)> {
    let lowered = raw.to_ascii_lowercase();
    let recognized = [
        ("https://mangadex.org/title/", "mangadex"),
        ("https://www.mangadex.org/title/", "mangadex"),
        ("http://mangadex.org/title/", "mangadex"),
        ("https://comicvine.gamespot.com/", "comicvine"),
        ("http://comicvine.gamespot.com/", "comicvine"),
        ("https://www.mangaupdates.com/series.html", "mangaupdates"),
        ("https://mangaupdates.com/series.html", "mangaupdates"),
    ]
    .iter()
    .find_map(|(prefix, kind)| lowered.starts_with(prefix).then_some(*kind));
    let url = match reqwest::Url::parse(raw) {
        Ok(url)
            if raw.len() <= 2048
                && !raw.contains('\\')
                && !raw.chars().any(char::is_control)
                && url.username().is_empty()
                && url.password().is_none() =>
        {
            url
        }
        _ => return recognized.map(|kind| (kind, None)),
    };
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.trim_start_matches("www.");
    let parts: Vec<_> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match host {
        "mangadex.org" if parts.first() == Some(&"title") => {
            Some(("mangadex", parts.get(1).and_then(|s| mangadex(s))))
        }
        "comicvine.gamespot.com" if parts.len() >= 2 => {
            Some(("comicvine", parts.last().and_then(|s| comicvine(s))))
        }
        "mangaupdates.com" if url.path() == "/series.html" => Some((
            "mangaupdates",
            url.query_pairs()
                .find(|(k, _)| k == "id")
                .and_then(|(_, v)| positive_number(&v)),
        )),
        _ => None,
    }
}

impl Library {
    pub async fn usable_identity_hints_for_file(
        &self,
        file_id: &str,
    ) -> Result<Vec<IdentityHint>, sqlx::Error> {
        self.file_identity_hints(file_id, true).await
    }

    pub async fn file_identity_hints(
        &self,
        file_id: &str,
        usable_only: bool,
    ) -> Result<Vec<IdentityHint>, sqlx::Error> {
        let rows = sqlx::query("SELECT h.kind,h.raw_value,h.value,h.status,h.source_field,h.container_path,h.extracted_at FROM library_files f JOIN scan_entries e ON e.root_id=f.root_id AND e.relative_path=f.relative_path AND e.signature=f.signature AND e.state='pending_association' JOIN identity_hints h ON h.entry_id=e.id AND h.library_file_id=f.id AND h.signature=e.signature WHERE f.id=? AND (?=0 OR h.status='usable') ORDER BY h.kind,h.raw_value,h.source_field")
            .bind(file_id).bind(usable_only).fetch_all(self.store.reader()).await?;
        Ok(rows
            .into_iter()
            .map(|r| IdentityHint {
                kind: r.get("kind"),
                raw_value: r.get("raw_value"),
                value: r.get("value"),
                status: r.get("status"),
                source_field: r.get("source_field"),
                container_path: r.get("container_path"),
                extracted_at: r.get("extracted_at"),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use crate::library::metadata::{EmbeddedMetadata, EmbeddedMetadataError};
    #[test]
    fn identity_hint_validation() {
        let cases = [
            ("<ISBN>0-306-40615-2</ISBN>", "isbn", "usable"),
            ("<ISBN>9780306406157</ISBN>", "isbn", "usable"),
            ("<EAN>4006381333931</EAN>", "gtin", "usable"),
            ("<ISBN>9780306406158</ISBN>", "isbn", "needs_review"),
            ("<ISBN>978030640615Z</ISBN>", "isbn", "needs_review"),
            (
                "<MangaDexID>f9c33607-9180-4ba6-b85c-e4b5faee7192</MangaDexID>",
                "mangadex",
                "usable",
            ),
            (
                "<ComicVineID>4050-bad</ComicVineID>",
                "comicvine",
                "needs_review",
            ),
            (
                "<Web>https://comicvine.gamespot.com/foo/4050-123/</Web>",
                "comicvine",
                "usable",
            ),
            (
                "<ISBN>123456789012345678901234567890123</ISBN>",
                "isbn",
                "needs_review",
            ),
        ];
        for (field, kind, status) in cases {
            let xml = format!("<ComicInfo>{field}</ComicInfo>");
            let hints = EmbeddedMetadata::parse(xml.as_bytes())
                .unwrap()
                .identity_hints("info/ComicInfo.xml", 123);
            assert_eq!(hints.len(), 1, "{field}");
            assert_eq!(
                (hints[0].kind.as_str(), hints[0].status.as_str()),
                (kind, status),
                "{field}"
            );
            assert_eq!(
                (hints[0].container_path.as_str(), hints[0].extracted_at),
                ("info/ComicInfo.xml", 123)
            );
        }
        let xml = b"<ComicInfo><ISBN>0306406152 9780306406157</ISBN></ComicInfo>";
        assert!(
            EmbeddedMetadata::parse(xml)
                .unwrap()
                .identity_hints("ComicInfo.xml", 1)
                .iter()
                .all(|hint| hint.status == "needs_review")
        );
        let long_url = format!(
            "<ComicInfo><Web>https://mangadex.org/title/{}</Web></ComicInfo>",
            "x".repeat(2049)
        );
        let hints = EmbeddedMetadata::parse_for_hints(long_url.as_bytes())
            .unwrap()
            .identity_hints("ComicInfo.xml", 1);
        assert_eq!(hints[0].status, "needs_review");
        assert!(matches!(
            EmbeddedMetadata::parse(long_url.as_bytes()),
            Err(EmbeddedMetadataError::LimitExceeded)
        ));
        let oversize = format!("<ComicInfo><ISBN>{}</ISBN></ComicInfo>", "1".repeat(5000));
        assert!(matches!(
            EmbeddedMetadata::parse(oversize.as_bytes()),
            Err(EmbeddedMetadataError::LimitExceeded)
        ));
    }
}
