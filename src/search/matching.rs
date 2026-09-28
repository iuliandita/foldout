//! Conservative matching defaults: exact evidence is required for every populated target field.
//! Unlisted formats and sources sort after configured entries; they are not rejected.

use crate::{
    catalog::wanted::UnitContext,
    catalog::{ContentType, DatePrecision},
    providers::release_evidence::{Coverage, Evidence, MediaFormat, ReleaseEvidence},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct Candidate<'a> {
    pub source: &'a str,
    /// Explicit only for an independently verified provider category mapping.
    pub content_type: Evidence<ContentType>,
    pub evidence: &'a ReleaseEvidence,
}

#[derive(Clone, Debug)]
pub struct MatchingPolicy<'a> {
    pub format_order: &'a [MediaFormat],
    pub source_priority: &'a [&'a str],
    pub discovery_complete: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Eligibility {
    Eligible,
    Unknown,
    Ineligible,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchField {
    Title,
    ContentType,
    Language,
    Run,
    Region,
    Publisher,
    Unit,
    Date,
    Format,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    Missing,
    Ambiguous,
    Conflict,
    Insufficient,
    CaseVariant,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MatchReason {
    pub field: MatchField,
    pub code: ReasonCode,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Evaluation {
    pub eligibility: Eligibility,
    pub reasons: Vec<MatchReason>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RankedCandidate {
    pub index: usize,
    pub format_rank: usize,
    pub source_rank: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Selection {
    Unknown,
    Review,
    Winner(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ranking {
    pub eligibility: Vec<Eligibility>,
    pub evaluations: Vec<Evaluation>,
    pub eligible: Vec<RankedCandidate>,
    pub selection: Selection,
}

pub fn eligibility(target: &UnitContext, candidate: &Candidate<'_>) -> Eligibility {
    evaluate(target, candidate).eligibility
}

pub fn evaluate(target: &UnitContext, candidate: &Candidate<'_>) -> Evaluation {
    let publication = &target.publication;
    let edition = &target.edition;
    let unit = &target.unit;
    let verdicts = [
        title_match(&publication.title, &candidate.evidence.title_stem),
        required(
            MatchField::ContentType,
            &publication.content_type,
            &candidate.content_type,
        ),
        required(
            MatchField::Language,
            &edition.language,
            &candidate.evidence.language,
        ),
        optional(
            MatchField::Run,
            publication.run_label.as_ref(),
            &candidate.evidence.run,
        ),
        optional(
            MatchField::Region,
            edition.region.as_ref(),
            &candidate.evidence.region,
        ),
        optional(
            MatchField::Publisher,
            edition.publisher.as_ref(),
            &candidate.evidence.publisher,
        ),
        unit_match(target, candidate.evidence),
        date_match(
            unit.date.as_ref(),
            unit.date_precision.as_ref(),
            &candidate.evidence.cover_date,
        ),
        required_format(&candidate.evidence.format),
    ];
    Evaluation {
        eligibility: verdicts
            .iter()
            .fold(Eligibility::Eligible, |state, verdict| {
                merge(state, verdict.eligibility)
            }),
        reasons: verdicts
            .into_iter()
            .filter_map(|verdict| verdict.reason)
            .collect(),
    }
}

pub fn rank(
    target: &UnitContext,
    candidates: &[Candidate<'_>],
    policy: &MatchingPolicy<'_>,
) -> Ranking {
    let evaluations: Vec<_> = candidates
        .iter()
        .map(|candidate| evaluate(target, candidate))
        .collect();
    let eligibility: Vec<_> = evaluations
        .iter()
        .map(|evaluation| evaluation.eligibility)
        .collect();
    let mut eligible: Vec<_> = candidates
        .iter()
        .enumerate()
        .filter(|(index, _)| eligibility[*index] == Eligibility::Eligible)
        .map(|(index, candidate)| RankedCandidate {
            index,
            format_rank: match &candidate.evidence.format {
                Evidence::Explicit(value) => position(policy.format_order, value),
                _ => unreachable!(),
            },
            source_rank: policy
                .source_priority
                .iter()
                .position(|source| *source == candidate.source)
                .unwrap_or(policy.source_priority.len()),
        })
        .collect();
    eligible.sort_by_key(|item| (item.format_rank, item.source_rank, item.index));
    let selection = if !policy.discovery_complete
        || eligibility.contains(&Eligibility::Unknown)
        || eligible.is_empty()
    {
        Selection::Unknown
    } else if eligible
        .get(1)
        .is_some_and(|second| same_rank(&eligible[0], second))
    {
        Selection::Review
    } else {
        Selection::Winner(eligible[0].index)
    };
    Ranking {
        eligibility,
        evaluations,
        eligible,
        selection,
    }
}

struct FieldVerdict {
    eligibility: Eligibility,
    reason: Option<MatchReason>,
}

impl FieldVerdict {
    fn with_field(mut self, field: MatchField) -> Self {
        self.reason = self.reason.map(|reason| MatchReason { field, ..reason });
        self
    }
}

fn verdict(eligibility: Eligibility, code: Option<ReasonCode>) -> FieldVerdict {
    FieldVerdict {
        eligibility,
        reason: code.map(|code| MatchReason {
            field: MatchField::Title,
            code,
        }),
    }
}

fn evidence_verdict<T>(field: MatchField, evidence: &Evidence<T>) -> FieldVerdict {
    match evidence {
        Evidence::Unknown => verdict(Eligibility::Unknown, Some(ReasonCode::Missing)),
        Evidence::Ambiguous => verdict(Eligibility::Unknown, Some(ReasonCode::Ambiguous)),
        Evidence::Explicit(_) => verdict(Eligibility::Eligible, None),
    }
    .with_field(field)
}

fn required<T: Eq>(field: MatchField, target: &T, evidence: &Evidence<T>) -> FieldVerdict {
    match evidence {
        Evidence::Explicit(value) if value == target => verdict(Eligibility::Eligible, None),
        Evidence::Explicit(_) => verdict(Eligibility::Ineligible, Some(ReasonCode::Conflict)),
        Evidence::Unknown => verdict(Eligibility::Unknown, Some(ReasonCode::Missing)),
        Evidence::Ambiguous => verdict(Eligibility::Unknown, Some(ReasonCode::Ambiguous)),
    }
    .with_field(field)
}

fn optional<T: Eq>(field: MatchField, target: Option<&T>, evidence: &Evidence<T>) -> FieldVerdict {
    target.map_or_else(
        || verdict(Eligibility::Eligible, None),
        |value| required(field, value, evidence),
    )
}

fn title_match(target: &String, evidence: &Evidence<String>) -> FieldVerdict {
    match evidence {
        Evidence::Explicit(value) if value == target => verdict(Eligibility::Eligible, None),
        Evidence::Explicit(value) if value.to_lowercase() == target.to_lowercase() => {
            verdict(Eligibility::Unknown, Some(ReasonCode::CaseVariant))
        }
        Evidence::Explicit(_) => verdict(Eligibility::Ineligible, Some(ReasonCode::Conflict)),
        Evidence::Unknown => verdict(Eligibility::Unknown, Some(ReasonCode::Missing)),
        Evidence::Ambiguous => verdict(Eligibility::Unknown, Some(ReasonCode::Ambiguous)),
    }
    .with_field(MatchField::Title)
}

fn required_format(evidence: &Evidence<MediaFormat>) -> FieldVerdict {
    match evidence {
        Evidence::Explicit(_) => verdict(Eligibility::Eligible, None),
        _ => evidence_verdict(MatchField::Format, evidence),
    }
}

fn unit_match(target: &UnitContext, evidence: &ReleaseEvidence) -> FieldVerdict {
    if evidence.coverage != Coverage::Exact {
        return verdict(Eligibility::Unknown, Some(ReasonCode::Insufficient))
            .with_field(MatchField::Unit);
    }
    let Evidence::Explicit(value) = &evidence.unit else {
        return evidence_verdict(MatchField::Unit, &evidence.unit);
    };
    if value.kind != target.unit.kind {
        return verdict(Eligibility::Ineligible, Some(ReasonCode::Conflict))
            .with_field(MatchField::Unit);
    }
    match (decimal(&target.unit.label), decimal(&value.canonical)) {
        (Some(left), Some(right)) if left == right => verdict(Eligibility::Eligible, None),
        (Some(_), Some(_)) => verdict(Eligibility::Ineligible, Some(ReasonCode::Conflict)),
        (None, _) if value.raw_label == target.unit.label => verdict(Eligibility::Eligible, None),
        _ => verdict(Eligibility::Unknown, Some(ReasonCode::Insufficient)),
    }
    .with_field(MatchField::Unit)
}

fn date_match(
    date: Option<&String>,
    precision: Option<&DatePrecision>,
    evidence: &Evidence<crate::providers::release_evidence::CoverDate>,
) -> FieldVerdict {
    let Some(date) = date else {
        return verdict(Eligibility::Eligible, None);
    };
    let Some(precision) = precision else {
        return verdict(Eligibility::Unknown, Some(ReasonCode::Insufficient))
            .with_field(MatchField::Date);
    };
    match evidence {
        Evidence::Explicit(value) if &value.value == date && &value.precision == precision => {
            verdict(Eligibility::Eligible, None)
        }
        Evidence::Explicit(value) if compatible_dates(date, &value.value) => {
            verdict(Eligibility::Unknown, Some(ReasonCode::Insufficient))
        }
        Evidence::Explicit(_) => verdict(Eligibility::Ineligible, Some(ReasonCode::Conflict)),
        _ => evidence_verdict(MatchField::Date, evidence),
    }
    .with_field(MatchField::Date)
}

fn compatible_dates(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('-'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('-'))
}

fn decimal(value: &str) -> Option<String> {
    let (whole, fraction) = value
        .split_once('.')
        .map_or((value, None), |(whole, fraction)| (whole, Some(fraction)));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.is_some_and(|value| {
            value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return None;
    }
    let whole = whole.trim_start_matches('0');
    let fraction = fraction
        .map(|value| value.trim_end_matches('0'))
        .filter(|value| !value.is_empty());
    Some(match fraction {
        Some(fraction) => format!(
            "{}.{}",
            if whole.is_empty() { "0" } else { whole },
            fraction
        ),
        None if whole.is_empty() => "0".into(),
        None => whole.into(),
    })
}

fn merge(left: Eligibility, right: Eligibility) -> Eligibility {
    if matches!(left, Eligibility::Ineligible) || matches!(right, Eligibility::Ineligible) {
        Eligibility::Ineligible
    } else if matches!(left, Eligibility::Unknown) || matches!(right, Eligibility::Unknown) {
        Eligibility::Unknown
    } else {
        Eligibility::Eligible
    }
}
fn position(values: &[MediaFormat], value: &MediaFormat) -> usize {
    values
        .iter()
        .position(|item| item == value)
        .unwrap_or(values.len())
}
fn same_rank(left: &RankedCandidate, right: &RankedCandidate) -> bool {
    left.format_rank == right.format_rank && left.source_rank == right.source_rank
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Edition, Publication, Unit, UnitKind};
    use crate::providers::release_evidence::{CoverDate, UnitEvidence};

    fn target(label: &str) -> UnitContext {
        UnitContext {
            publication: Publication {
                id: "p".into(),
                content_type: ContentType::Comic,
                title: "Pilot".into(),
                sort_title: "Pilot".into(),
                run_label: None,
                title_locked: false,
                known_unit_count: None,
            },
            edition: Edition {
                id: "e".into(),
                publication_id: "p".into(),
                language: "en".into(),
                region: None,
                publisher: None,
            },
            unit: Unit {
                id: "u".into(),
                edition_id: "e".into(),
                label: label.into(),
                kind: UnitKind::Issue,
                sort_key: None,
                date: None,
                date_precision: None,
            },
        }
    }
    fn evidence(label: &str) -> ReleaseEvidence {
        ReleaseEvidence {
            language: Evidence::Explicit("en".into()),
            region: Evidence::Unknown,
            publisher: Evidence::Unknown,
            run: Evidence::Unknown,
            title_stem: Evidence::Explicit("Pilot".into()),
            unit: Evidence::Explicit(UnitEvidence {
                raw_label: label.into(),
                canonical: label.into(),
                kind: UnitKind::Issue,
            }),
            cover_date: Evidence::Unknown,
            format: Evidence::Explicit(MediaFormat::Cbz),
            coverage: Coverage::Exact,
        }
    }
    fn candidate<'a>(evidence: &'a ReleaseEvidence) -> Candidate<'a> {
        Candidate {
            source: "a",
            content_type: Evidence::Explicit(ContentType::Comic),
            evidence,
        }
    }

    fn complete_target() -> UnitContext {
        let mut value = target("1");
        value.publication.run_label = Some("Run".into());
        value.edition.region = Some("US".into());
        value.edition.publisher = Some("Press".into());
        value.unit.date = Some("2024-01".into());
        value.unit.date_precision = Some(DatePrecision::Month);
        value
    }

    fn complete_evidence() -> ReleaseEvidence {
        let mut value = evidence("1");
        value.run = Evidence::Explicit("Run".into());
        value.region = Evidence::Explicit("US".into());
        value.publisher = Evidence::Explicit("Press".into());
        value.cover_date = Evidence::Explicit(CoverDate {
            value: "2024-01".into(),
            precision: DatePrecision::Month,
        });
        value
    }

    #[test]
    fn conflicts_reject_and_missing_required_evidence_is_unknown() {
        let target = target("12.5");
        let mut item = evidence("12.5");
        assert_eq!(
            eligibility(&target, &candidate(&item)),
            Eligibility::Eligible
        );
        item.title_stem = Evidence::Explicit("Other".into());
        let evaluation = evaluate(&target, &candidate(&item));
        assert_eq!(evaluation.eligibility, Eligibility::Ineligible);
        assert!(evaluation.reasons.contains(&MatchReason {
            field: MatchField::Title,
            code: ReasonCode::Conflict,
        }));
        item.title_stem = Evidence::Unknown;
        assert_eq!(
            eligibility(&target, &candidate(&item)),
            Eligibility::Unknown
        );
    }

    #[test]
    fn unit_and_date_evidence_stay_exact() {
        let numeric_target = target("12.50");
        let item = evidence("12.5");
        assert_eq!(
            eligibility(&numeric_target, &candidate(&item)),
            Eligibility::Eligible
        );
        let text_target = target("Special");
        assert_eq!(
            eligibility(&text_target, &candidate(&item)),
            Eligibility::Unknown
        );
        let mut dated = target("12.5");
        dated.unit.date = Some("2024-01".into());
        dated.unit.date_precision = Some(DatePrecision::Month);
        let mut item = evidence("12.5");
        item.cover_date = Evidence::Explicit(CoverDate {
            value: "2024-01-02".into(),
            precision: DatePrecision::Day,
        });
        let evaluation = evaluate(&dated, &candidate(&item));
        assert_eq!(evaluation.eligibility, Eligibility::Unknown);
        assert!(evaluation.reasons.contains(&MatchReason {
            field: MatchField::Date,
            code: ReasonCode::Insufficient,
        }));
        item.cover_date = Evidence::Explicit(CoverDate {
            value: "2024-02-02".into(),
            precision: DatePrecision::Day,
        });
        assert_eq!(
            eligibility(&dated, &candidate(&item)),
            Eligibility::Ineligible
        );
        item.title_stem = Evidence::Explicit("PILOT".into());
        assert_eq!(
            eligibility(&target("12.5"), &candidate(&item)),
            Eligibility::Unknown
        );
    }

    #[test]
    fn ranking_requires_complete_known_discovery_and_reviews_ties() {
        let target = target("1");
        let one = evidence("1");
        let two = evidence("1");
        let candidates = [candidate(&one), candidate(&two)];
        let policy = MatchingPolicy {
            format_order: &[MediaFormat::Cbz],
            source_priority: &["a"],
            discovery_complete: true,
        };
        assert_eq!(
            rank(&target, &candidates, &policy).selection,
            Selection::Review
        );
        let policy = MatchingPolicy {
            discovery_complete: false,
            ..policy
        };
        assert_eq!(
            rank(&target, &candidates[..1], &policy).selection,
            Selection::Unknown
        );
    }

    #[test]
    fn all_supported_types_and_unit_kinds_require_exact_evidence() {
        for content_type in [
            ContentType::Comic,
            ContentType::Manga,
            ContentType::Magazine,
        ] {
            for kind in [UnitKind::Issue, UnitKind::Chapter, UnitKind::Volume] {
                let mut target = target("1");
                target.publication.content_type = content_type.clone();
                target.unit.kind = kind.clone();
                let mut item = evidence("1");
                item.unit = Evidence::Explicit(UnitEvidence {
                    raw_label: "1".into(),
                    canonical: "1".into(),
                    kind,
                });
                let candidate = Candidate {
                    source: "a",
                    content_type: Evidence::Explicit(content_type.clone()),
                    evidence: &item,
                };
                assert_eq!(eligibility(&target, &candidate), Eligibility::Eligible);
            }
        }
    }

    #[test]
    fn explicit_conflicts_dominate_other_missing_evidence() {
        for (name, field) in [
            ("title", MatchField::Title),
            ("type", MatchField::ContentType),
            ("language", MatchField::Language),
            ("run", MatchField::Run),
            ("region", MatchField::Region),
            ("publisher", MatchField::Publisher),
            ("kind", MatchField::Unit),
            ("unit", MatchField::Unit),
            ("date", MatchField::Date),
        ] {
            let target = complete_target();
            let mut item = complete_evidence();
            let mut content_type = ContentType::Comic;
            match name {
                "title" => item.title_stem = Evidence::Explicit("Elsewhere".into()),
                "type" => content_type = ContentType::Manga,
                "language" => item.language = Evidence::Explicit("de".into()),
                "run" => item.run = Evidence::Explicit("Other".into()),
                "region" => item.region = Evidence::Explicit("GB".into()),
                "publisher" => item.publisher = Evidence::Explicit("Other".into()),
                "kind" => {
                    item.unit = Evidence::Explicit(UnitEvidence {
                        raw_label: "1".into(),
                        canonical: "1".into(),
                        kind: UnitKind::Volume,
                    })
                }
                "unit" => {
                    item.unit = Evidence::Explicit(UnitEvidence {
                        raw_label: "2".into(),
                        canonical: "2".into(),
                        kind: UnitKind::Issue,
                    })
                }
                "date" => {
                    item.cover_date = Evidence::Explicit(CoverDate {
                        value: "2024-02".into(),
                        precision: DatePrecision::Month,
                    })
                }
                _ => unreachable!(),
            }
            item.format = Evidence::Unknown;
            let candidate = Candidate {
                source: "a",
                content_type: Evidence::Explicit(content_type),
                evidence: &item,
            };
            let evaluation = evaluate(&target, &candidate);
            assert_eq!(evaluation.eligibility, Eligibility::Ineligible, "{name}");
            assert!(
                evaluation.reasons.contains(&MatchReason {
                    field,
                    code: ReasonCode::Conflict
                }),
                "{name}"
            );
        }
    }

    #[test]
    fn empty_optional_target_fields_do_not_become_constraints() {
        let target = target("1");
        let mut item = evidence("1");
        item.run = Evidence::Explicit("Other run".into());
        item.region = Evidence::Explicit("GB".into());
        item.publisher = Evidence::Explicit("Other press".into());
        item.cover_date = Evidence::Explicit(CoverDate {
            value: "2024-02".into(),
            precision: DatePrecision::Month,
        });
        assert_eq!(
            eligibility(&target, &candidate(&item)),
            Eligibility::Eligible
        );
    }

    #[test]
    fn ambiguous_required_evidence_has_a_typed_unknown_reason() {
        let target = target("1");
        let mut item = evidence("1");
        item.language = Evidence::Ambiguous;
        let evaluation = evaluate(&target, &candidate(&item));
        assert_eq!(evaluation.eligibility, Eligibility::Unknown);
        assert!(evaluation.reasons.contains(&MatchReason {
            field: MatchField::Language,
            code: ReasonCode::Ambiguous
        }));
    }

    #[test]
    fn decimal_labels_normalize_without_float_and_invalid_values_stay_unknown() {
        for (target_label, raw_label, canonical, expected) in [
            ("001.20", "1.2", "1.2", Eligibility::Eligible),
            ("000.00", "0", "0", Eligibility::Eligible),
            ("1", "1", "one", Eligibility::Unknown),
            ("Special", "Special", "special", Eligibility::Eligible),
            ("Special", "Other", "other", Eligibility::Unknown),
        ] {
            let target = target(target_label);
            let mut item = evidence(raw_label);
            item.unit = Evidence::Explicit(UnitEvidence {
                raw_label: raw_label.into(),
                canonical: canonical.into(),
                kind: UnitKind::Issue,
            });
            assert_eq!(
                eligibility(&target, &candidate(&item)),
                expected,
                "{target_label}"
            );
        }
    }

    #[test]
    fn ranking_uses_format_then_source_and_never_autoselects_unknown() {
        let target = target("1");
        let mut cbr = evidence("1");
        cbr.format = Evidence::Explicit(MediaFormat::Cbr);
        let cbz = evidence("1");
        let policy = MatchingPolicy {
            format_order: &[MediaFormat::Cbz, MediaFormat::Cbr],
            source_priority: &["first", "second"],
            discovery_complete: true,
        };
        let candidates = [
            Candidate {
                source: "first",
                content_type: Evidence::Explicit(ContentType::Comic),
                evidence: &cbr,
            },
            Candidate {
                source: "second",
                content_type: Evidence::Explicit(ContentType::Comic),
                evidence: &cbz,
            },
        ];
        assert_eq!(
            rank(&target, &candidates, &policy).selection,
            Selection::Winner(1)
        );

        let tie = [candidate(&cbz), candidate(&cbz)];
        assert_eq!(rank(&target, &tie, &policy).selection, Selection::Review);
        assert_eq!(
            rank(&target, &tie.into_iter().rev().collect::<Vec<_>>(), &policy).selection,
            Selection::Review
        );

        let mut unknown = evidence("1");
        unknown.title_stem = Evidence::Unknown;
        let blocked = [candidate(&cbz), candidate(&unknown)];
        assert_eq!(
            rank(&target, &blocked, &policy).selection,
            Selection::Unknown
        );
        assert_eq!(
            rank(
                &target,
                &[candidate(&cbz)],
                &MatchingPolicy {
                    discovery_complete: false,
                    ..policy
                }
            )
            .selection,
            Selection::Unknown
        );
        assert_eq!(rank(&target, &[], &policy).selection, Selection::Unknown);

        let mut rejected = evidence("1");
        rejected.title_stem = Evidence::Explicit("Other".into());
        assert_eq!(
            rank(&target, &[candidate(&rejected)], &policy).selection,
            Selection::Unknown
        );
    }
}
