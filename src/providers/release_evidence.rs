use std::fmt;

use serde::{Deserialize, Serialize};

use super::ContentType;
use crate::catalog::{DatePrecision, UnitKind, validate_date};

pub const MAX_TITLE_BYTES: usize = 1024;
const MAX_VALUE_BYTES: usize = 256;

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum Evidence<T> {
    Unknown,
    Explicit(T),
    Ambiguous,
}

impl<T: fmt::Debug> fmt::Debug for Evidence<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => f.write_str("Unknown"),
            Self::Explicit(value) => f.debug_tuple("Explicit").field(value).finish(),
            Self::Ambiguous => f.write_str("Ambiguous"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaFormat {
    Cbz,
    Cbr,
    Pdf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UnitEvidence {
    pub raw_label: String,
    pub canonical: String,
    pub kind: UnitKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CoverDate {
    pub value: String,
    pub precision: DatePrecision,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Exact,
    Collection,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReleaseEvidence {
    pub language: Evidence<String>,
    pub region: Evidence<String>,
    pub publisher: Evidence<String>,
    pub run: Evidence<String>,
    pub title_stem: Evidence<String>,
    pub unit: Evidence<UnitEvidence>,
    pub cover_date: Evidence<CoverDate>,
    pub format: Evidence<MediaFormat>,
    pub coverage: Coverage,
}

impl ReleaseEvidence {
    fn unknown() -> Self {
        Self {
            language: Evidence::Unknown,
            region: Evidence::Unknown,
            publisher: Evidence::Unknown,
            run: Evidence::Unknown,
            title_stem: Evidence::Unknown,
            unit: Evidence::Unknown,
            cover_date: Evidence::Unknown,
            format: Evidence::Unknown,
            coverage: Coverage::Unknown,
        }
    }
}

/// Parses only labels and delimiters that make evidence explicit. It does not inspect
/// RSS timestamps, protocols, URLs, GUIDs, XML, or provider errors.
pub fn parse_release_evidence(input: &str, content_type: ContentType) -> ReleaseEvidence {
    if !safe_release_title(input) {
        return ReleaseEvidence::unknown();
    }
    let Some(segments) = bracket_segments(input) else {
        return ReleaseEvidence::unknown();
    };
    let mut evidence = ReleaseEvidence::unknown();
    let lower = input.to_ascii_lowercase();
    let mut unrecognized_segment = false;
    let mut tagged_spans = Vec::new();
    let mut recognized_spans = Vec::new();
    for segment in &segments {
        let trimmed = segment.value.trim();
        let lower_segment = trimmed.to_ascii_lowercase();
        let tagged_segment = ["language:", "region:", "publisher:", "run:"]
            .iter()
            .any(|prefix| lower_segment.starts_with(prefix));
        if tagged_segment {
            tagged_spans.push(segment.span);
        }
        let mut recognized = false;
        if let Some(value) = tagged(&lower_segment, trimmed, "language:") {
            merge(&mut evidence.language, value);
            recognized = true;
        }
        if let Some(value) = tagged(&lower_segment, trimmed, "region:") {
            merge(&mut evidence.region, value);
            recognized = true;
        }
        if let Some(value) = tagged(&lower_segment, trimmed, "publisher:") {
            merge(&mut evidence.publisher, value);
            recognized = true;
        }
        if let Some(value) = tagged(&lower_segment, trimmed, "run:") {
            merge(&mut evidence.run, value);
            recognized = true;
        }
        if let Some(format) = bracket_format(&lower_segment) {
            merge(&mut evidence.format, format);
            recognized = true;
        }
        if content_type == ContentType::Magazine
            && matches!(trimmed.len(), 7 | 10)
            && validate_date(trimmed).is_ok()
        {
            recognized = true;
        }
        if recognized {
            recognized_spans.push(segment.span);
        }
        unrecognized_segment |= !recognized;
    }
    if let Some(format) = filename_format(&lower) {
        merge(&mut evidence.format, format);
    }

    let semantic_input = masked_input(input, &tagged_spans);
    let semantic_lower = semantic_input.to_ascii_lowercase();
    let dates = if content_type == ContentType::Magazine {
        cover_dates(&semantic_input)
    } else {
        Vec::new()
    };

    let units = explicit_units(input, &semantic_input, &semantic_lower);
    let malformed_units = units.malformed || (unrecognized_segment && !units.values.is_empty());
    let ranged_units = units.range;
    let title_unit_start = units.values.iter().map(|value| value.start).min();
    let date_suffixes = date_suffixes(input, &dates, &units.values, &recognized_spans);
    for (start, date) in &dates {
        if date_suffixes
            .iter()
            .any(|(evidence_start, _)| evidence_start == start)
        {
            merge(&mut evidence.cover_date, date.clone());
        }
    }
    let title_date_start = date_suffixes.iter().map(|(_, cut)| *cut).min();
    if malformed_units {
        evidence.unit = Evidence::Ambiguous;
        evidence.coverage = Coverage::Unknown;
    } else if contains_collection(&semantic_lower) || ranged_units {
        evidence.coverage = Coverage::Collection;
    } else {
        for candidate in units.values {
            merge_unit(&mut evidence.unit, candidate.evidence);
        }
        evidence.coverage = match evidence.unit {
            Evidence::Explicit(_) => Coverage::Exact,
            Evidence::Unknown | Evidence::Ambiguous => Coverage::Unknown,
        };
    }

    evidence.title_stem = if malformed_units || unrecognized_segment {
        Evidence::Unknown
    } else {
        title_stem(
            input,
            title_unit_start.into_iter().chain(title_date_start).min(),
        )
    };
    evidence
}

fn merge<T: Eq>(slot: &mut Evidence<T>, value: T) {
    match slot {
        Evidence::Unknown => *slot = Evidence::Explicit(value),
        Evidence::Explicit(current) if *current == value => {}
        Evidence::Explicit(_) => *slot = Evidence::Ambiguous,
        Evidence::Ambiguous => {}
    }
}

fn merge_unit(slot: &mut Evidence<UnitEvidence>, value: UnitEvidence) {
    match slot {
        Evidence::Unknown => *slot = Evidence::Explicit(value),
        Evidence::Explicit(current)
            if current.kind == value.kind && current.canonical == value.canonical => {}
        Evidence::Explicit(_) => *slot = Evidence::Ambiguous,
        Evidence::Ambiguous => {}
    }
}

pub(crate) fn safe_release_title(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    input.len() <= MAX_TITLE_BYTES
        && !input.chars().any(char::is_control)
        && !input.contains('@')
        && !["://", "apikey=", "token=", "password=", "authorization:"]
            .iter()
            .any(|needle| lower.contains(needle))
}

#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}

struct BracketSegment<'a> {
    span: Span,
    value: &'a str,
}

fn bracket_segments(input: &str) -> Option<Vec<BracketSegment<'_>>> {
    let mut values = Vec::new();
    let mut start: Option<(char, usize, usize)> = None;
    for (index, character) in input.char_indices() {
        match character {
            '[' | '(' | '{' => {
                if start.is_some() {
                    return None;
                }
                start = Some((character, index, index + character.len_utf8()));
            }
            ']' | ')' | '}' => {
                let (opening, outer_start, begin) = start.take()?;
                let expected = match opening {
                    '[' => ']',
                    '(' => ')',
                    '{' => '}',
                    _ => unreachable!(),
                };
                if character != expected || begin > index {
                    return None;
                }
                values.push(BracketSegment {
                    span: Span {
                        start: outer_start,
                        end: index + character.len_utf8(),
                    },
                    value: &input[begin..index],
                });
            }
            _ => {}
        }
    }
    start.is_none().then_some(values)
}

fn masked_input(input: &str, spans: &[Span]) -> String {
    let mut value = input.as_bytes().to_vec();
    for span in spans {
        value[span.start..span.end].fill(b' ');
    }
    String::from_utf8(value).expect("masking complete UTF-8 spans preserves UTF-8")
}

fn tagged(lower: &str, original: &str, prefix: &str) -> Option<String> {
    let value = lower.strip_prefix(prefix)?.trim();
    if value.is_empty() || value.len() > MAX_VALUE_BYTES || !safe_value(value) {
        return None;
    }
    let offset = original.len() - original.trim_start().len() + prefix.len();
    let original_value = original.get(offset..)?.trim();
    safe_value(original_value).then(|| original_value.to_owned())
}

fn safe_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_VALUE_BYTES
        && value.chars().all(|character| {
            character.is_alphanumeric()
                || character.is_whitespace()
                || matches!(character, '-' | '_' | '.' | '\'' | '’' | '&')
        })
}

fn title_stem(input: &str, suffix_start: Option<usize>) -> Evidence<String> {
    let trimmed = input.trim_end();
    let format_start =
        filename_format(&trimmed.to_ascii_lowercase()).map(|_| trimmed.len().saturating_sub(4));
    let end = suffix_start
        .into_iter()
        .chain(format_start)
        .min()
        .unwrap_or(trimmed.len());
    let title = input[..end]
        .trim()
        .trim_matches(['.', '-', '_', ' '])
        .to_owned();
    if title.len() > MAX_VALUE_BYTES
        || !safe_title_value(&title)
        || !title.chars().any(char::is_alphabetic)
    {
        Evidence::Unknown
    } else {
        Evidence::Explicit(title)
    }
}

fn date_suffixes(
    input: &str,
    dates: &[(usize, CoverDate)],
    units: &[ParsedUnit],
    recognized_spans: &[Span],
) -> Vec<(usize, usize)> {
    if dates.is_empty() {
        return Vec::new();
    }
    let trimmed = input.trim_end();
    let mut suffix_spans = recognized_spans.to_vec();
    suffix_spans.extend(units.iter().map(|unit| Span {
        start: unit.start,
        end: unit.end,
    }));
    suffix_spans.extend(dates.iter().map(|(start, date)| Span {
        start: *start,
        end: *start + date.value.len(),
    }));
    if filename_format(&trimmed.to_ascii_lowercase()).is_some() {
        suffix_spans.push(Span {
            start: trimmed.len().saturating_sub(4),
            end: trimmed.len(),
        });
    }
    let masked = masked_input(input, &suffix_spans);
    dates
        .iter()
        .filter_map(|(start, _)| {
            let cut = recognized_spans
                .iter()
                .find(|span| span.start <= *start && *start < span.end)
                .map_or(*start, |span| span.start);
            masked[cut..trimmed.len()]
                .chars()
                .all(|character| character.is_whitespace() || matches!(character, '.' | '-' | '_'))
                .then_some((*start, cut))
        })
        .collect()
}

fn safe_title_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_VALUE_BYTES
        && value.chars().all(|character| {
            character.is_alphanumeric()
                || character.is_whitespace()
                || matches!(
                    character,
                    '-' | '_' | '.' | '\'' | '’' | '&' | ':' | ',' | '!' | '?' | '/'
                )
        })
}

fn bracket_format(value: &str) -> Option<MediaFormat> {
    match value {
        "cbz" => Some(MediaFormat::Cbz),
        "cbr" => Some(MediaFormat::Cbr),
        "pdf" => Some(MediaFormat::Pdf),
        _ => None,
    }
}

fn filename_format(value: &str) -> Option<MediaFormat> {
    value
        .trim_end()
        .strip_suffix(".cbz")
        .map(|_| MediaFormat::Cbz)
        .or_else(|| {
            value
                .trim_end()
                .strip_suffix(".cbr")
                .map(|_| MediaFormat::Cbr)
        })
        .or_else(|| {
            value
                .trim_end()
                .strip_suffix(".pdf")
                .map(|_| MediaFormat::Pdf)
        })
}

fn contains_collection(value: &str) -> bool {
    ["range", "omnibus", "complete", "collection", "bundle"]
        .iter()
        .any(|word| bounded_word(value, word))
}

fn bounded_word(value: &str, word: &str) -> bool {
    value.match_indices(word).any(|(index, _)| {
        let before = value[..index].chars().next_back();
        let after = value[index + word.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

struct ParsedUnit {
    start: usize,
    end: usize,
    evidence: UnitEvidence,
}

#[derive(Default)]
struct UnitScan {
    values: Vec<ParsedUnit>,
    range: bool,
    malformed: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum UnitTail {
    Exact,
    Range,
    Malformed,
}

fn explicit_units(original: &str, input: &str, lower: &str) -> UnitScan {
    let mut result = UnitScan::default();
    for (label, kind) in [
        ("#", UnitKind::Issue),
        ("issue", UnitKind::Issue),
        ("ch.", UnitKind::Chapter),
        ("chapter", UnitKind::Chapter),
        ("vol.", UnitKind::Volume),
        ("volume", UnitKind::Volume),
    ] {
        for (index, _) in lower.match_indices(label) {
            if input[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
            {
                continue;
            }
            let begin = index + label.len();
            let (canonical, end) = match decimal_at(input, begin) {
                Some(value) => value,
                None => {
                    let value_start = skip_whitespace(input, begin);
                    let next = input[value_start..].chars().next();
                    if label == "#"
                        || (next != Some('#') && leading_token_has_digit(&input[value_start..]))
                    {
                        result.malformed = true;
                    }
                    continue;
                }
            };
            match unit_tail(input, end) {
                UnitTail::Range => result.range = true,
                UnitTail::Malformed => result.malformed = true,
                UnitTail::Exact => result.values.push(ParsedUnit {
                    start: index,
                    end,
                    evidence: UnitEvidence {
                        raw_label: original[index..end].trim().to_owned(),
                        canonical,
                        kind: kind.clone(),
                    },
                }),
            }
        }
    }
    result
}

fn leading_token_has_digit(value: &str) -> bool {
    value
        .chars()
        .take_while(|character| character.is_alphanumeric() || matches!(character, '.' | '-' | '_'))
        .any(|character| character.is_ascii_digit())
}

fn decimal_at(input: &str, index: usize) -> Option<(String, usize)> {
    let mut index = skip_whitespace(input, index);
    let start = index;
    while input.as_bytes().get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    if index == start || index - start > 16 {
        return None;
    }
    if input.as_bytes().get(index) == Some(&b'.')
        && input
            .as_bytes()
            .get(index + 1)
            .is_some_and(u8::is_ascii_digit)
    {
        let point = index;
        index += 1;
        let fraction = index;
        while input.as_bytes().get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == fraction || index - fraction > 16 {
            return None;
        }
        let whole = input[start..point].trim_start_matches('0');
        let fraction = input[fraction..index].trim_end_matches('0');
        let canonical = if fraction.is_empty() {
            if whole.is_empty() {
                "0".to_owned()
            } else {
                whole.to_owned()
            }
        } else {
            format!(
                "{}.{}",
                if whole.is_empty() { "0" } else { whole },
                fraction
            )
        };
        return Some((canonical, index));
    }
    let whole = input[start..index].trim_start_matches('0');
    Some((
        if whole.is_empty() {
            "0".to_owned()
        } else {
            whole.to_owned()
        },
        index,
    ))
}

fn skip_whitespace(input: &str, mut index: usize) -> usize {
    while let Some(character) = input[index..].chars().next() {
        if !character.is_whitespace() {
            break;
        }
        index += character.len_utf8();
    }
    index
}

fn unit_tail(input: &str, end: usize) -> UnitTail {
    let range_start = skip_whitespace(input, end);
    if let Some(separator) = range_separator(&input[range_start..]) {
        let right_start = skip_whitespace(input, range_start + separator);
        let Some((_, right_end)) = decimal_at(input, right_start) else {
            return UnitTail::Malformed;
        };
        return if clean_numeric_end(input, right_end) {
            UnitTail::Range
        } else {
            UnitTail::Malformed
        };
    }
    if clean_numeric_end(input, end) {
        UnitTail::Exact
    } else {
        UnitTail::Malformed
    }
}

fn range_separator(value: &str) -> Option<usize> {
    let first = value.chars().next()?;
    if matches!(first, '-' | '‐' | '‑' | '‒' | '–' | '—' | '−') {
        return Some(first.len_utf8());
    }
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("to")
        && value[2..]
            .chars()
            .next()
            .is_none_or(|character| !character.is_alphanumeric())
    {
        Some(2)
    } else {
        None
    }
}

fn numeric_terminator(input: &str, end: usize) -> bool {
    if end == input.len() {
        return true;
    }
    let remainder = &input[end..];
    if remainder.starts_with(']') || remainder.starts_with(')') || remainder.starts_with('}') {
        return true;
    }
    matches!(
        remainder.trim_end().to_ascii_lowercase().as_str(),
        ".cbz" | ".cbr" | ".pdf"
    )
}

fn clean_numeric_end(input: &str, end: usize) -> bool {
    if numeric_terminator(input, end) {
        return true;
    }
    let next = skip_whitespace(input, end);
    next > end
        && (next == input.len()
            || input[next..].starts_with('[')
            || input[next..].starts_with('(')
            || input[next..].starts_with('{')
            || matches!(
                input[next..].trim_end().to_ascii_lowercase().as_str(),
                ".cbz" | ".cbr" | ".pdf"
            ))
}

fn cover_dates(input: &str) -> Vec<(usize, CoverDate)> {
    let mut values = Vec::new();
    for (start, character) in input.char_indices() {
        if !character.is_ascii_digit()
            || input[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        let day_end = start + 10;
        if let Some(value) = input.get(start..day_end)
            && date_shape(value, 10)
        {
            if token_ends_at(input, day_end)
                && let Ok(precision) = validate_date(value)
            {
                values.push((
                    start,
                    CoverDate {
                        value: value.to_owned(),
                        precision,
                    },
                ));
            }
            continue;
        }
        let month_end = start + 7;
        let Some(value) = input.get(start..month_end) else {
            continue;
        };
        if !date_shape(value, 7)
            || input[month_end..].starts_with('-')
            || !token_ends_at(input, month_end)
        {
            continue;
        }
        if let Ok(precision) = validate_date(value) {
            values.push((
                start,
                CoverDate {
                    value: value.to_owned(),
                    precision,
                },
            ));
        }
    }
    values
}

fn date_shape(value: &str, length: usize) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == length
        && bytes.iter().enumerate().all(|(index, byte)| {
            if index == 4 || index == 7 {
                *byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        })
}

fn token_ends_at(input: &str, end: usize) -> bool {
    input[end..]
        .chars()
        .next()
        .is_none_or(|character| !character.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_explicit_units_for_each_content_type() {
        for (title, content_type, kind, canonical, format) in [
            (
                "Comic #0012.50 [language:en].cbz",
                ContentType::Comic,
                UnitKind::Issue,
                "12.5",
                MediaFormat::Cbz,
            ),
            (
                "Manga Ch. 12.5 [cbr]",
                ContentType::Manga,
                UnitKind::Chapter,
                "12.5",
                MediaFormat::Cbr,
            ),
            (
                "Magazine Issue 7 [2024-02-29].pdf",
                ContentType::Magazine,
                UnitKind::Issue,
                "7",
                MediaFormat::Pdf,
            ),
        ] {
            let evidence = parse_release_evidence(title, content_type);
            assert_eq!(evidence.coverage, Coverage::Exact);
            assert!(
                matches!(evidence.unit, Evidence::Explicit(UnitEvidence { kind: actual, canonical: ref value, .. }) if actual == kind && value == canonical)
            );
            assert_eq!(evidence.format, Evidence::Explicit(format));
        }
    }

    #[test]
    fn range_and_bare_year_never_become_exact_evidence() {
        for title in [
            "Series #1-5 [cbz]",
            "Series #1 - 5 [cbz]",
            "Series #1–5 [cbz]",
            "Series #1 — 5 [cbz]",
            "Series #1 to 5 [cbz]",
        ] {
            let range = parse_release_evidence(title, ContentType::Comic);
            assert_eq!(range.coverage, Coverage::Collection, "accepted {title}");
            assert_eq!(range.unit, Evidence::Unknown, "accepted {title}");
        }
        let year = parse_release_evidence("Magazine 2024 [cbz]", ContentType::Magazine);
        assert_eq!(year.cover_date, Evidence::Unknown);
    }

    #[test]
    fn numeric_suffixes_and_malformed_competitors_are_never_exact() {
        for title in [
            "Comic #1A [cbz]",
            "Comic #1 A [cbz]",
            "Comic #1½ [cbz]",
            "Comic #1.2.3 [cbz]",
            "Comic #1™ [cbz]",
            "Comic #A1 [cbz]",
            "Comic Issue A1 [cbz]",
            "Comic Issue 12th [cbz]",
            "Comic #1 #1A [cbz]",
            "Comic #1 - later [cbz]",
        ] {
            let evidence = parse_release_evidence(title, ContentType::Comic);
            assert_eq!(evidence.coverage, Coverage::Unknown, "accepted {title}");
            assert_eq!(evidence.unit, Evidence::Ambiguous, "accepted {title}");
        }
    }

    #[test]
    fn decimal_termination_allows_only_a_real_format_suffix() {
        for (title, expected) in [("Comic #1.cbz", "1"), ("Comic #1.5.pdf", "1.5")] {
            let evidence = parse_release_evidence(title, ContentType::Comic);
            assert_eq!(evidence.coverage, Coverage::Exact);
            assert!(matches!(
                evidence.unit,
                Evidence::Explicit(UnitEvidence { canonical: ref actual, .. }) if actual == expected
            ));
        }
        let evidence = parse_release_evidence("Comic #1.foo", ContentType::Comic);
        assert_eq!(evidence.coverage, Coverage::Unknown);
        assert_eq!(evidence.unit, Evidence::Ambiguous);
    }

    #[test]
    fn malformed_or_mismatched_brackets_fail_closed() {
        for title in [
            "Title #1 [language:en)",
            "Title #1 (run:2016]",
            "Title #1 [[cbz]",
            "Title #1 ] [cbz]",
            "Title #1 (language:en",
        ] {
            assert_eq!(
                parse_release_evidence(title, ContentType::Comic),
                ReleaseEvidence::unknown(),
                "accepted {title}"
            );
        }
    }

    #[test]
    fn unicode_tag_values_are_preserved() {
        let evidence = parse_release_evidence(
            "進撃の巨人 Ch. 12.5 [language:日本語] [region:España] [publisher:Éditions Glénat] [cbz]",
            ContentType::Manga,
        );
        assert_eq!(evidence.language, Evidence::Explicit("日本語".into()));
        assert_eq!(evidence.region, Evidence::Explicit("España".into()));
        assert_eq!(
            evidence.publisher,
            Evidence::Explicit("Éditions Glénat".into())
        );
        assert_eq!(evidence.coverage, Coverage::Exact);
    }

    #[test]
    fn magazine_dates_are_bounded_title_evidence() {
        let evidence =
            parse_release_evidence("Magazine 2001-01 Issue 7 [pdf]", ContentType::Magazine);
        assert_eq!(
            evidence.cover_date,
            Evidence::Explicit(CoverDate {
                value: "2001-01".into(),
                precision: DatePrecision::Month,
            })
        );
        assert_eq!(evidence.title_stem, Evidence::Explicit("Magazine".into()));

        let embedded = parse_release_evidence("Magazine X2001-01Y [pdf]", ContentType::Magazine);
        assert_eq!(embedded.cover_date, Evidence::Unknown);
        let invalid = parse_release_evidence("Magazine 2001-13 [pdf]", ContentType::Magazine);
        assert_eq!(invalid.cover_date, Evidence::Unknown);
        let conflicting =
            parse_release_evidence("Magazine 2001-01 2001-02 [pdf]", ContentType::Magazine);
        assert_eq!(conflicting.cover_date, Evidence::Ambiguous);
    }

    #[test]
    fn semantic_scans_ignore_recognized_tag_values() {
        let unit =
            parse_release_evidence("Magazine [publisher:Issue 7] [pdf]", ContentType::Magazine);
        assert_eq!(unit.unit, Evidence::Unknown);
        assert_eq!(unit.coverage, Coverage::Unknown);

        let date = parse_release_evidence("Magazine [run:2024-01] [pdf]", ContentType::Magazine);
        assert_eq!(date.run, Evidence::Explicit("2024-01".into()));
        assert_eq!(date.cover_date, Evidence::Unknown);

        let collection = parse_release_evidence(
            "Magazine Issue 7 [publisher:Complete Comics] [pdf]",
            ContentType::Magazine,
        );
        assert_eq!(collection.coverage, Coverage::Exact);
        assert!(matches!(collection.unit, Evidence::Explicit(_)));
    }

    #[test]
    fn invalid_day_never_downgrades_to_month_evidence() {
        let evidence =
            parse_release_evidence("Magazine 2023-02-29 Issue 7 [pdf]", ContentType::Magazine);
        assert_eq!(evidence.cover_date, Evidence::Unknown);
        assert_eq!(
            evidence.title_stem,
            Evidence::Explicit("Magazine 2023-02-29".into())
        );
    }

    #[test]
    fn embedded_date_text_does_not_truncate_the_title() {
        let evidence =
            parse_release_evidence("A 2024-01 Story Issue 7 [pdf]", ContentType::Magazine);
        assert_eq!(evidence.cover_date, Evidence::Unknown);
        assert_eq!(
            evidence.title_stem,
            Evidence::Explicit("A 2024-01 Story".into())
        );
        assert_eq!(evidence.coverage, Coverage::Exact);

        let bracketed =
            parse_release_evidence("A [2024-01] Story Issue 7 [pdf]", ContentType::Magazine);
        assert_eq!(bracketed.cover_date, Evidence::Unknown);
        assert_eq!(bracketed.title_stem, Evidence::Unknown);
        assert_eq!(bracketed.coverage, Coverage::Exact);
    }

    #[test]
    fn ordinary_title_words_are_not_unit_markers() {
        for (title, stem) in [
            ("The Issue of Batman #1 [cbz]", "The Issue of Batman"),
            ("Volume Control #2 [cbz]", "Volume Control"),
            ("Chapter House #3 [cbz]", "Chapter House"),
        ] {
            let evidence = parse_release_evidence(title, ContentType::Comic);
            assert_eq!(evidence.title_stem, Evidence::Explicit(stem.into()));
            assert_eq!(evidence.coverage, Coverage::Exact);
        }

        let uncertain = parse_release_evidence("Title [Deluxe] #1 [cbz]", ContentType::Comic);
        assert_eq!(uncertain.title_stem, Evidence::Unknown);
        assert_eq!(uncertain.unit, Evidence::Ambiguous);
        assert_eq!(uncertain.coverage, Coverage::Unknown);
    }

    #[test]
    fn transport_never_implies_media_format() {
        let evidence = parse_release_evidence("Comic #1 [language:English]", ContentType::Comic);
        assert_eq!(evidence.format, Evidence::Unknown);
    }

    #[test]
    fn unknown_format_and_conflicting_tags_are_conservative() {
        let evidence =
            parse_release_evidence("Title #2 [language:en] [language:es]", ContentType::Comic);
        assert_eq!(evidence.language, Evidence::Ambiguous);
        assert_eq!(evidence.format, Evidence::Unknown);
    }

    #[test]
    fn serialization_and_debug_never_retain_unsafe_input() {
        let evidence = parse_release_evidence(
            "https://user:secret@example.test/file.cbz",
            ContentType::Comic,
        );
        let json = serde_json::to_string(&evidence).unwrap();
        let debug = format!("{evidence:?}");
        assert!(!json.contains("secret") && !debug.contains("secret"));
        assert_eq!(evidence.title_stem, Evidence::Unknown);
    }
}
