//! Explicit, bounded enrichment hints. Nothing here matches catalog identities or coverage.

use std::{collections::BTreeMap, fs::File, sync::Arc};

use quick_xml::{
    Reader,
    escape::{resolve_xml_entity, unescape_with},
    events::Event,
};
use serde::Serialize;

use crate::reader::archive::{ArchiveDecoder, ArchiveError, MAX_COMIC_INFO_BYTES};

const MAX_DEPTH: usize = 16;
const MAX_FIELD_BYTES: usize = 4096;
const MAX_WEB_URLS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum EmbeddedMetadataSource {
    ComicInfo,
}

/// Untrusted, trimmed raw values, including unknown number/date/language/manga values.
/// These are suggestions for explicit preview, never authoritative catalog data.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EmbeddedMetadata {
    pub source: EmbeddedMetadataSource,
    pub title: Option<String>,
    pub series: Option<String>,
    pub number: Option<String>,
    pub volume: Option<String>,
    pub year: Option<String>,
    pub month: Option<String>,
    pub day: Option<String>,
    pub language_iso: Option<String>,
    pub publisher: Option<String>,
    pub manga: Option<String>,
    /// HTTP(S) URLs only. Never fetched or converted into provider identities here.
    pub web_urls: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum EmbeddedMetadataError {
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error("ComicInfo.xml exceeds a metadata limit")]
    LimitExceeded,
    #[error("ComicInfo.xml is malformed or uses forbidden XML constructs")]
    InvalidXml,
}

impl EmbeddedMetadata {
    /// Call only for explicit enrichment. Reuse the caller's decoder worker pool.
    /// Missing ComicInfo.xml is `Ok(None)`; malformed or ambiguous metadata is an error.
    pub async fn read(
        decoder: &ArchiveDecoder,
        file: Arc<File>,
    ) -> Result<Option<Self>, EmbeddedMetadataError> {
        decoder
            .comic_info(file)
            .await?
            .as_deref()
            .map(Self::parse)
            .transpose()
    }

    /// Parses UTF-8 XML under the same byte limit as archive extraction.
    /// Only the five predefined XML entities and legal numeric references are decoded.
    /// DTDs and custom entities (including in attributes) are rejected.
    /// Unknown elements are ignored but still subject to XML and nesting checks.
    pub fn parse(xml: &[u8]) -> Result<Self, EmbeddedMetadataError> {
        use EmbeddedMetadataError::{InvalidXml, LimitExceeded};
        if xml.len() > MAX_COMIC_INFO_BYTES {
            return Err(LimitExceeded);
        }
        let xml = std::str::from_utf8(xml).map_err(|_| InvalidXml)?;
        if !xml.chars().all(xml_character) {
            return Err(InvalidXml);
        }
        let mut reader = Reader::from_str(xml);
        reader.config_mut().enable_all_checks(true);
        reader.config_mut().expand_empty_elements = true;
        let mut depth = 0usize;
        let mut root_seen = false;
        let mut declaration_seen = false;
        let mut fields = BTreeMap::new();
        let mut active: Option<String> = None;
        loop {
            match reader.read_event().map_err(|_| InvalidXml)? {
                Event::Start(tag) => {
                    let name = tag.name();
                    let name = name.as_ref();
                    // Attribute duplicate checks are linear in the preceding attribute count.
                    for (index, attr) in tag.attributes().enumerate() {
                        if index == 32 {
                            return Err(LimitExceeded);
                        }
                        let attr = attr.map_err(|_| InvalidXml)?;
                        if attr.value.contains('<') {
                            return Err(InvalidXml);
                        }
                        let decoded = unescape_with(&attr.value, resolve_xml_entity)
                            .map_err(|_| InvalidXml)?;
                        if !decoded.chars().all(xml_character) {
                            return Err(InvalidXml);
                        }
                    }
                    if depth == 0 {
                        if root_seen || name != "ComicInfo" {
                            return Err(InvalidXml);
                        }
                        root_seen = true;
                    } else if depth == 1 && field_limit(name).is_some() {
                        if fields.insert(name.to_owned(), String::new()).is_some() {
                            return Err(InvalidXml);
                        }
                        active = Some(name.to_owned());
                    } else if active.is_some() {
                        return Err(InvalidXml);
                    }
                    depth += 1;
                    if depth > MAX_DEPTH {
                        return Err(LimitExceeded);
                    }
                }
                Event::End(_) => {
                    if depth == 2 {
                        active = None;
                    }
                    depth = depth.checked_sub(1).ok_or(InvalidXml)?;
                }
                Event::Text(text) => {
                    append_text(text.as_ref(), depth, active.as_deref(), &mut fields)?
                }
                Event::GeneralRef(reference) => {
                    if depth == 0 {
                        return Err(InvalidXml);
                    }
                    let mut buffer = [0u8; 4];
                    let decoded = match reference.resolve_char_ref().map_err(|_| InvalidXml)? {
                        Some(character) if xml_character(character) => {
                            character.encode_utf8(&mut buffer)
                        }
                        Some(_) => return Err(InvalidXml),
                        None => resolve_xml_entity(reference.as_ref()).ok_or(InvalidXml)?,
                    };
                    append_text(decoded, depth, active.as_deref(), &mut fields)?;
                }
                Event::CData(text) => {
                    if depth == 0 {
                        return Err(InvalidXml);
                    }
                    append_text(text.as_ref(), depth, active.as_deref(), &mut fields)?;
                }
                Event::Decl(decl) => {
                    if root_seen
                        || declaration_seen
                        || decl.version().map_err(|_| InvalidXml)? != "1.0"
                    {
                        return Err(InvalidXml);
                    }
                    if let Some(encoding) = decl.encoding()
                        && !encoding
                            .map_err(|_| InvalidXml)?
                            .eq_ignore_ascii_case("UTF-8")
                    {
                        return Err(InvalidXml);
                    }
                    declaration_seen = true;
                }
                Event::Comment(_) => {}
                Event::Eof => break,
                // No custom expansion, external resolution, processing instructions, or DTDs.
                _ => return Err(InvalidXml),
            }
        }
        if !root_seen || depth != 0 {
            return Err(InvalidXml);
        }
        let mut take = |name: &str| {
            fields
                .remove(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let web = take("Web");
        let mut web_urls = Vec::new();
        if let Some(web) = web {
            for raw in web.split_whitespace() {
                if raw.len() > 2048 {
                    return Err(LimitExceeded);
                }
                if let Ok(url) = reqwest::Url::parse(raw)
                    && matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && !raw.contains('\\')
                    && !raw.chars().any(char::is_control)
                {
                    if web_urls.len() == MAX_WEB_URLS {
                        return Err(LimitExceeded);
                    }
                    web_urls.push(raw.to_owned());
                }
            }
        }
        Ok(Self {
            source: EmbeddedMetadataSource::ComicInfo,
            title: take("Title"),
            series: take("Series"),
            number: take("Number"),
            volume: take("Volume"),
            year: take("Year"),
            month: take("Month"),
            day: take("Day"),
            language_iso: take("LanguageISO"),
            publisher: take("Publisher"),
            manga: take("Manga"),
            web_urls,
        })
    }

    /// A calendar-valid date at exactly the supplied precision, or None.
    /// Invalid or incomplete components never become a fabricated date or a coarser date.
    pub fn date(&self) -> Option<String> {
        let number = |value: &str| {
            (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                .then(|| value.parse::<u32>().ok())
                .flatten()
        };
        let year = number(self.year.as_deref()?)?;
        if !(1..=9999).contains(&year) {
            return None;
        }
        let Some(month) = self.month.as_deref() else {
            return self.day.is_none().then(|| format!("{year:04}"));
        };
        let month = number(month)?;
        if !(1..=12).contains(&month) {
            return None;
        }
        let Some(day) = self.day.as_deref() else {
            return Some(format!("{year:04}-{month:02}"));
        };
        let day = number(day)?;
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let maximum = match month {
            2 if leap => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        (1..=maximum)
            .contains(&day)
            .then(|| format!("{year:04}-{month:02}-{day:02}"))
    }
}

fn xml_character(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

fn field_limit(name: &str) -> Option<usize> {
    match name {
        "Title" | "Series" | "Publisher" => Some(MAX_FIELD_BYTES),
        "Number" | "Volume" | "Year" | "Month" | "Day" | "LanguageISO" | "Manga" => Some(64),
        "Web" => Some(MAX_WEB_URLS * 2049),
        _ => None,
    }
}

fn append_text(
    text: &str,
    depth: usize,
    active: Option<&str>,
    fields: &mut BTreeMap<String, String>,
) -> Result<(), EmbeddedMetadataError> {
    if depth <= 1 && !text.bytes().all(|b| b.is_ascii_whitespace()) {
        return Err(EmbeddedMetadataError::InvalidXml);
    }
    if let Some(name) = active {
        let value = fields.get_mut(name).expect("active field exists");
        if value.len().saturating_add(text.len()) > field_limit(name).expect("known field") {
            return Err(EmbeddedMetadataError::LimitExceeded);
        }
        value.push_str(text);
    }
    Ok(())
}
