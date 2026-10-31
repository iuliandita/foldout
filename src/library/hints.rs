use std::collections::{BTreeMap, BTreeSet};

use quick_xml::{
    Reader,
    escape::{resolve_xml_entity, unescape_with},
    events::Event,
};
use serde::Serialize;
use sqlx::Row;

use super::roots::Library;
use crate::reader::archive::MAX_COMIC_INFO_BYTES;

const MAX_HINTS: usize = 16;
const MAX_RAW_BYTES: usize = 4096;

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

fn xml_character(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

fn kind_for_field(field: &str) -> Option<&'static str> {
    match field {
        "ISBN" | "ISBN10" | "ISBN13" => Some("isbn"),
        "GTIN" | "GTIN13" | "EAN" | "EAN13" => Some("gtin"),
        "ComicVineID" => Some("comicvine"),
        "MangaDexID" => Some("mangadex"),
        "MangaUpdatesID" => Some("mangaupdates"),
        "Web" | "URL" => Some("url"),
        _ => None,
    }
}

struct Field {
    name: String,
    raw: String,
    malformed: bool,
}

fn append(field: &mut Field, text: &str) {
    let remaining = MAX_RAW_BYTES.saturating_sub(field.raw.len());
    let mut end = text.len().min(remaining);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    field.raw.push_str(&text[..end]);
    field.malformed |= end < text.len();
}

pub(crate) fn parse_identity_hints(
    xml: &[u8],
    container: &str,
    extracted_at: i64,
) -> Result<Vec<IdentityHint>, ()> {
    if xml.len() > MAX_COMIC_INFO_BYTES {
        return Err(());
    }
    let input = std::str::from_utf8(xml).map_err(|_| ())?;
    if !input.chars().all(xml_character) {
        return Err(());
    }
    let mut reader = Reader::from_str(input);
    reader.config_mut().enable_all_checks(true);
    reader.config_mut().expand_empty_elements = true;
    let mut depth = 0usize;
    let mut root = false;
    let mut active: Option<Field> = None;
    let mut fields = Vec::new();
    let mut overflow = false;
    let mut fields_overflow = false;
    loop {
        match reader.read_event().map_err(|_| ())? {
            Event::Start(tag) => {
                for (index, attribute) in tag.attributes().enumerate() {
                    if index == 32 {
                        return Err(());
                    }
                    let attribute = attribute.map_err(|_| ())?;
                    if attribute.value.contains('<') {
                        return Err(());
                    }
                    let decoded =
                        unescape_with(&attribute.value, resolve_xml_entity).map_err(|_| ())?;
                    if !decoded.chars().all(xml_character) {
                        return Err(());
                    }
                }
                let name = tag.name();
                if depth == 0 {
                    if root || name.as_ref() != "ComicInfo" {
                        return Err(());
                    }
                    root = true;
                } else if depth == 1 {
                    if kind_for_field(name.as_ref()).is_some() {
                        active = Some(Field {
                            name: name.as_ref().to_owned(),
                            raw: String::new(),
                            malformed: false,
                        });
                    }
                } else if let Some(field) = &mut active {
                    field.malformed = true;
                }
                depth += 1;
                if depth > 16 {
                    return Err(());
                }
            }
            Event::End(_) => {
                if depth == 2
                    && let Some(field) = active.take()
                {
                    if fields.len() < MAX_HINTS + 1 {
                        fields.push(field);
                    } else {
                        fields_overflow = true;
                    }
                }
                depth = depth.checked_sub(1).ok_or(())?;
            }
            Event::Text(text) => {
                if depth == 0 && !text.as_ref().bytes().all(|b| b.is_ascii_whitespace()) {
                    return Err(());
                }
                if let Some(field) = &mut active {
                    append(field, text.as_ref());
                }
            }
            Event::CData(text) => {
                if let Some(field) = &mut active {
                    append(field, text.as_ref());
                }
            }
            Event::GeneralRef(reference) => {
                if depth == 0 {
                    return Err(());
                }
                let mut buffer = [0u8; 4];
                let decoded = match reference.resolve_char_ref().map_err(|_| ())? {
                    Some(c) if xml_character(c) => c.encode_utf8(&mut buffer),
                    Some(_) => return Err(()),
                    None => resolve_xml_entity(reference.as_ref()).ok_or(())?,
                };
                if let Some(field) = &mut active {
                    append(field, decoded);
                }
            }
            Event::Decl(_) | Event::Comment(_) => {}
            Event::Eof => break,
            _ => return Err(()),
        }
    }
    if !root || depth != 0 {
        return Err(());
    }
    let mut counts = BTreeMap::new();
    for field in &fields {
        *counts.entry(field.name.as_str()).or_insert(0usize) += 1;
    }
    let mut hints = Vec::new();
    for field in &fields {
        let field_kind = kind_for_field(&field.name).expect("recognized field");
        let split: Box<dyn Iterator<Item = &str> + '_> = if field_kind == "url" {
            Box::new(field.raw.split_whitespace())
        } else {
            Box::new(
                field
                    .raw
                    .split([',', ';', '\n'])
                    .map(str::trim)
                    .filter(|v| !v.is_empty()),
            )
        };
        for (index, raw) in split.enumerate() {
            if index == MAX_HINTS {
                overflow = true;
                break;
            }
            let identified = match field_kind {
                "isbn" => Some(("isbn", book_number(raw, true))),
                "gtin" => Some(("gtin", book_number(raw, false))),
                "comicvine" => Some(("comicvine", comicvine(raw))),
                "mangadex" => Some(("mangadex", mangadex(raw))),
                "mangaupdates" => Some(("mangaupdates", positive_number(raw))),
                _ => url_hint(raw),
            };
            if let Some((kind, mut value)) = identified {
                if hints.len() == MAX_HINTS {
                    overflow = true;
                    break;
                }
                if field.malformed || counts.get(field.name.as_str()).copied().unwrap_or(0) > 1 {
                    value = None;
                }
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
                    source_field: field.name.clone(),
                    container_path: container.into(),
                    extracted_at,
                });
            }
        }
        if overflow {
            break;
        }
    }
    overflow |= fields_overflow;
    let mut values: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for hint in &hints {
        if let Some(value) = &hint.value {
            values
                .entry(identity_group(&hint.kind, value))
                .or_default()
                .insert(value);
        }
    }
    let ambiguous: BTreeSet<String> = values
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(k, _)| k.to_owned())
        .collect();
    for hint in &mut hints {
        if hint
            .value
            .as_ref()
            .is_some_and(|v| ambiguous.contains(identity_group(&hint.kind, v)))
        {
            hint.status = "needs_review".into();
            hint.value = None;
        }
    }
    if overflow {
        for hint in &mut hints {
            hint.status = "needs_review".into();
            hint.value = None;
        }
        hints.truncate(MAX_HINTS - 1);
        hints.push(IdentityHint {
            kind: "too_many_hints".into(),
            raw_value: "too_many_hints".into(),
            value: None,
            status: "needs_review".into(),
            source_field: "ComicInfo".into(),
            container_path: container.into(),
            extracted_at,
        });
    }
    Ok(hints)
}

pub(crate) fn identity_group<'a>(kind: &'a str, value: &str) -> &'a str {
    if kind == "gtin" && (value.starts_with("978") || value.starts_with("979")) {
        "isbn"
    } else {
        kind
    }
}

pub(crate) fn normalized_raw(kind: &str, raw: &str) -> Option<String> {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return url_hint(raw).and_then(|(found, value)| (found == kind).then_some(value).flatten());
    }
    match kind {
        "isbn" => book_number(raw, true),
        "gtin" => book_number(raw, false),
        "comicvine" => comicvine(raw),
        "mangadex" => mangadex(raw),
        "mangaupdates" => positive_number(raw),
        _ => None,
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
    Some(format!(
        "4050-{}",
        positive_number(raw.strip_prefix("4050-")?)?
    ))
}
fn mangadex(raw: &str) -> Option<String> {
    let id = uuid::Uuid::parse_str(raw).ok()?;
    (raw.len() == 36 && !id.is_nil()).then(|| id.hyphenated().to_string())
}
fn book_number(raw: &str, isbn: bool) -> Option<String> {
    if raw.len() > 64
        || !raw.bytes().all(|b| {
            b.is_ascii_digit()
                || b == b'-'
                || b.is_ascii_whitespace()
                || (isbn && (b == b'X' || b == b'x'))
        })
    {
        return None;
    }
    let digits: String = raw
        .chars()
        .filter(|c| *c != '-' && !c.is_ascii_whitespace())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let bytes = digits.as_bytes();
    if isbn && bytes.len() == 10 {
        let sum = bytes.iter().enumerate().try_fold(0u32, |sum, (i, b)| {
            let n = if i == 9 && *b == b'X' {
                10
            } else if b.is_ascii_digit() {
                u32::from(*b - b'0')
            } else {
                return None;
            };
            Some(sum + (10 - i as u32) * n)
        })?;
        if sum % 11 != 0 {
            return None;
        }
        let base = format!("978{}", &digits[..9]);
        let sum: u32 = base
            .bytes()
            .enumerate()
            .map(|(i, b)| u32::from(b - b'0') * if i % 2 == 0 { 1 } else { 3 })
            .sum();
        return Some(format!("{base}{}", (10 - sum % 10) % 10));
    }
    if bytes.len() != 13
        || !bytes.iter().all(u8::is_ascii_digit)
        || (isbn && !digits.starts_with("978") && !digits.starts_with("979"))
    {
        return None;
    }
    let sum: u32 = bytes
        .iter()
        .enumerate()
        .map(|(i, b)| u32::from(*b - b'0') * if i % 2 == 0 { 1 } else { 3 })
        .sum();
    sum.is_multiple_of(10).then_some(digits)
}
fn url_hint(raw: &str) -> Option<(&'static str, Option<String>)> {
    let lowered = raw.to_ascii_lowercase();
    let recognized = [
        ("https://mangadex.org/title/", "mangadex"),
        ("https://www.mangadex.org/title/", "mangadex"),
        ("http://mangadex.org/title/", "mangadex"),
        ("https://comicvine.gamespot.com/", "comicvine"),
        ("http://comicvine.gamespot.com/", "comicvine"),
        ("https://www.mangaupdates.com/series", "mangaupdates"),
        ("https://mangaupdates.com/series", "mangaupdates"),
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
        "mangaupdates.com" if url.path().starts_with("/series") => Some(("mangaupdates", None)),
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
        let rows = sqlx::query("SELECT h.kind,h.raw_value,h.value,h.status,h.source_field,h.container_path,h.extracted_at FROM library_files f JOIN scan_entries e ON e.root_id=f.root_id AND e.relative_path=f.relative_path AND e.signature=f.signature AND e.state='pending_association' JOIN identity_hints h ON h.entry_id=e.id AND h.library_file_id=f.id AND h.signature=e.signature WHERE f.id=? AND (?=0 OR h.status='usable') ORDER BY h.kind,h.raw_value,h.source_field LIMIT 16")
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
