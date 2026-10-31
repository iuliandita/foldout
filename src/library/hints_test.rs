use super::hints::parse_identity_hints;
use super::metadata::EmbeddedMetadata;

fn hints(fields: &str) -> Vec<super::hints::IdentityHint> {
    parse_identity_hints(
        format!("<ComicInfo>{fields}</ComicInfo>").as_bytes(),
        "ComicInfo.xml",
        42,
    )
    .unwrap()
}

#[test]
fn identity_validation_normalization_and_ambiguity() {
    for (input, expected, status) in [
        (
            "<ISBN>0 306 40615 2</ISBN>",
            Some("9780306406157"),
            "usable",
        ),
        (
            "<ISBN>0-8044-2957-x</ISBN>",
            Some("9780804429573"),
            "usable",
        ),
        (
            "<ISBN>9791090636071</ISBN>",
            Some("9791090636071"),
            "usable",
        ),
        ("<ISBN>4006381333931</ISBN>", None, "needs_review"),
        ("<ISBN>9780306406158</ISBN>", None, "needs_review"),
        ("<EAN>4006381333931</EAN>", Some("4006381333931"), "usable"),
        ("<MangaDexID>bad</MangaDexID>", None, "needs_review"),
        (
            "<Web>https://mangadex.org/title/not-a-uuid</Web>",
            None,
            "needs_review",
        ),
        (
            "<Web>https://mangadex.org/title/f9c33607-9180-4ba6-b85c-e4b5faee7192</Web>",
            Some("f9c33607-9180-4ba6-b85c-e4b5faee7192"),
            "usable",
        ),
        (
            "<Web>https://www.mangaupdates.com/series.html?id=123</Web>",
            None,
            "needs_review",
        ),
        (
            "<MangaUpdatesID>123</MangaUpdatesID>",
            Some("123"),
            "usable",
        ),
    ] {
        let result = hints(input);
        assert_eq!(result.len(), 1, "{input}");
        assert_eq!(result[0].value.as_deref(), expected, "{input}");
        assert_eq!(result[0].status, status, "{input}");
    }
    let equivalent =
        hints("<ISBN>0306406152</ISBN><ISBN13>9780306406157</ISBN13><EAN>9780306406157</EAN>");
    assert_eq!(equivalent.len(), 3);
    assert!(
        equivalent
            .iter()
            .all(|h| h.status == "usable" && h.value.as_deref() == Some("9780306406157"))
    );
    let ambiguous = hints("<ISBN>0306406152</ISBN><ISBN13>9791090636071</ISBN13>");
    assert!(
        ambiguous
            .iter()
            .all(|h| h.status == "needs_review" && h.value.is_none())
    );
}

#[test]
fn malformed_fields_are_reviewed_and_hint_count_is_bounded() {
    let fields = format!(
        "<ISBN>0306406152</ISBN><ISBN><nested>bad</nested></ISBN><URL>{}</URL>",
        "https://mangadex.org/title/".to_owned() + &"x".repeat(5000)
    );
    assert!(EmbeddedMetadata::parse(format!("<ComicInfo>{fields}</ComicInfo>").as_bytes()).is_ok());
    let items = hints(&fields);
    assert_eq!(items.len(), 3);
    assert!(
        items
            .iter()
            .all(|h| h.status == "needs_review" && h.value.is_none())
    );
    let many = hints(&"<MangaUpdatesID>123</MangaUpdatesID>".repeat(1000));
    assert_eq!(many.len(), 16);
    assert_eq!(
        many.iter().filter(|h| h.kind == "too_many_hints").count(),
        1
    );
    let tokens = hints(&format!(
        "<Web>{}</Web>",
        "https://mangadex.org/title/not-a-uuid ".repeat(1000)
    ));
    assert_eq!(tokens.len(), 16);
    assert_eq!(
        tokens.iter().filter(|h| h.kind == "too_many_hints").count(),
        1
    );
    assert!(
        parse_identity_hints(
            b"<ComicInfo><Other>&invalid;</Other></ComicInfo>",
            "ComicInfo.xml",
            1
        )
        .is_err()
    );
    let oversized = vec![b' '; crate::reader::archive::MAX_COMIC_INFO_BYTES + 1];
    assert!(parse_identity_hints(&oversized, "ComicInfo.xml", 1).is_err());
}
