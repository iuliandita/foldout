use std::{fs::File, io::Write, sync::Arc};

use libraryd::{
    library::metadata::{EmbeddedMetadata, EmbeddedMetadataError, EmbeddedMetadataSource},
    reader::archive::{ArchiveDecoder, ArchiveError, MAX_COMIC_INFO_BYTES},
};

fn parse(xml: &str) -> EmbeddedMetadata {
    EmbeddedMetadata::parse(xml.as_bytes()).unwrap()
}

#[test]
fn raw_hints_preserve_decimal_issues_languages_and_unknown_values() {
    let hints = parse(
        r#"<?xml version="1.0" encoding="utf-8"?>
        <ComicInfo xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
          <Title> First story </Title><Series>Example</Series><Number>001.50</Number>
          <Volume>Special</Volume><Year>2024</Year><Month>2</Month><Day>29</Day>
          <LanguageISO>pt-BR</LanguageISO><Publisher>Example Press</Publisher>
          <Manga>YesAndRightToLeft</Manga><Web>https://example.org/story http://example.net/</Web>
          <Unknown><Title>This is not the title</Title></Unknown>
        </ComicInfo>"#,
    );
    assert_eq!(hints.source, EmbeddedMetadataSource::ComicInfo);
    assert_eq!(hints.title.as_deref(), Some("First story"));
    assert_eq!(hints.series.as_deref(), Some("Example"));
    assert_eq!(hints.number.as_deref(), Some("001.50"));
    assert_eq!(hints.volume.as_deref(), Some("Special"));
    assert_eq!(hints.language_iso.as_deref(), Some("pt-BR"));
    assert_eq!(hints.publisher.as_deref(), Some("Example Press"));
    assert_eq!(hints.manga.as_deref(), Some("YesAndRightToLeft"));
    assert_eq!(hints.date().as_deref(), Some("2024-02-29"));
    assert_eq!(
        hints.web_urls,
        ["https://example.org/story", "http://example.net/"]
    );
}

#[test]
fn absent_fields_and_partial_dates_are_not_invented() {
    let empty = parse("<ComicInfo/>");
    assert_eq!(empty.title, None);
    assert_eq!(empty.language_iso, None);
    assert_eq!(empty.date(), None);
    assert!(empty.web_urls.is_empty());
    for (fields, expected) in [
        ("<Year>2023</Year>", "2023"),
        ("<Year>2023</Year><Month>9</Month>", "2023-09"),
        (
            "<Year>2000</Year><Month>2</Month><Day>29</Day>",
            "2000-02-29",
        ),
    ] {
        assert_eq!(
            parse(&format!("<ComicInfo>{fields}</ComicInfo>"))
                .date()
                .as_deref(),
            Some(expected)
        );
    }
}

#[test]
fn invalid_dates_stay_raw_and_do_not_degrade_to_valid_partial_dates() {
    for fields in [
        "<Year>2023</Year><Month>2</Month><Day>29</Day>",
        "<Year>1900</Year><Month>2</Month><Day>29</Day>",
        "<Year>2024</Year><Month>4</Month><Day>31</Day>",
        "<Year>2024</Year><Month>13</Month>",
        "<Year>2024</Year><Month>0</Month>",
        "<Year>2024</Year><Day>1</Day>",
        "<Year>0</Year>",
        "<Year>-1</Year>",
        "<Year>unknown</Year>",
        "<Month>2</Month><Day>3</Day>",
    ] {
        assert_eq!(
            parse(&format!("<ComicInfo>{fields}</ComicInfo>")).date(),
            None,
            "{fields}"
        );
    }
    assert_eq!(
        parse("<ComicInfo><Year>unknown</Year></ComicInfo>")
            .year
            .as_deref(),
        Some("unknown")
    );
}

#[test]
fn malicious_or_ambiguous_xml_is_rejected() {
    for xml in [
        "<!DOCTYPE ComicInfo><ComicInfo/>",
        "<!DOCTYPE ComicInfo [<!ENTITY x SYSTEM 'file:///etc/passwd'>]><ComicInfo><Title>&x;</Title></ComicInfo>",
        "<ComicInfo><Title>&unknown;</Title></ComicInfo>",
        "<ComicInfo><Title>&nbsp;</Title></ComicInfo>",
        "<ComicInfo value='&unknown;'/>",
        "<ComicInfo><Unknown>&unknown;</Unknown></ComicInfo>",
        "<ComicInfo><Title>a</Title><Title>b</Title></ComicInfo>",
        "<ComicInfo><Title/><Title>b</Title></ComicInfo>",
        "<ComicInfo><Title><b>nested</b></Title></ComicInfo>",
        "<ComicInfo><Title>mismatch</Series></ComicInfo>",
        "<ComicInfo>",
        "<Wrong/>",
        "<ComicInfo/><ComicInfo/>",
        "text<ComicInfo/>",
        "<ComicInfo/>text",
        "<ComicInfo>text</ComicInfo>",
        "<?xml version='1.0' encoding='UTF-16'?><ComicInfo/>",
        "<ComicInfo><?xml version='1.0'?></ComicInfo>",
        "<?fetch url='https://example.org'?><ComicInfo/>",
        "<ComicInfo><Title>nul\0</Title></ComicInfo>",
        "<ComicInfo a='1' a='2'/>",
        "<ComicInfo><!-- bad -- comment --></ComicInfo>",
    ] {
        assert!(
            EmbeddedMetadata::parse(xml.as_bytes()).is_err(),
            "accepted {xml}"
        );
    }
    assert!(EmbeddedMetadata::parse(b"<ComicInfo>\xff</ComicInfo>").is_err());
}

#[test]
fn predefined_and_numeric_references_decode_once_in_text_and_attributes() {
    let hints = parse(
        r#"<ComicInfo note="&lt;&gt;&amp;&apos;&quot;&#65;&#x1F600;">
        <Title>&lt;A &amp; B&gt; &quot;quoted&quot; &apos;single&apos; &#65;&#x1F600;</Title>
        <Publisher>A &amp; B</Publisher><Number>&#49;&#x32;.5</Number>
        <Series>&amp;unknown; &amp;#65;</Series>
        <Web>https://example.org/?a=1&amp;b=2</Web>
        <Unknown>&amp;&#65;</Unknown>
        </ComicInfo>"#,
    );
    assert_eq!(
        hints.title.as_deref(),
        Some("<A & B> \"quoted\" 'single' A\u{1f600}")
    );
    assert_eq!(hints.publisher.as_deref(), Some("A & B"));
    assert_eq!(hints.number.as_deref(), Some("12.5"));
    assert_eq!(hints.series.as_deref(), Some("&unknown; &#65;"));
    assert_eq!(hints.web_urls, ["https://example.org/?a=1&b=2"]);
}

#[test]
fn invalid_references_are_rejected_in_text_unknown_fields_and_attributes() {
    for reference in [
        "&unknown;",
        "&nbsp;",
        "&amp",
        "&#;",
        "&#x;",
        "&#0;",
        "&#1;",
        "&#xB;",
        "&#xD800;",
        "&#xFFFE;",
        "&#xFFFF;",
        "&#x110000;",
        "&#999999999999999999999;",
        "&#+65;",
        "&#x+41;",
    ] {
        for xml in [
            format!("<ComicInfo><Title>{reference}</Title></ComicInfo>"),
            format!("<ComicInfo><Unknown>{reference}</Unknown></ComicInfo>"),
            format!("<ComicInfo note='{reference}'/>"),
        ] {
            assert!(
                EmbeddedMetadata::parse(xml.as_bytes()).is_err(),
                "accepted {xml}"
            );
        }
    }
    assert!(EmbeddedMetadata::parse(b"&#32;<ComicInfo/>").is_err());
    assert!(EmbeddedMetadata::parse(b"<!DOCTYPE ComicInfo [<!ENTITY amp 'override'>]><ComicInfo><Title>&amp;</Title></ComicInfo>").is_err());
}

#[test]
fn decoded_references_respect_field_byte_limits() {
    let bounded = format!(
        "<ComicInfo><Title>{}</Title></ComicInfo>",
        "&#x1F600;".repeat(1024)
    );
    assert_eq!(parse(&bounded).title.unwrap().len(), 4096);
    let oversized = format!(
        "<ComicInfo><Title>{}</Title></ComicInfo>",
        "&#x1F600;".repeat(1025)
    );
    assert!(matches!(
        EmbeddedMetadata::parse(oversized.as_bytes()),
        Err(EmbeddedMetadataError::LimitExceeded)
    ));
}

#[test]
fn byte_field_nesting_and_url_limits_are_enforced() {
    let large = vec![b' '; MAX_COMIC_INFO_BYTES + 1];
    assert!(matches!(
        EmbeddedMetadata::parse(&large),
        Err(EmbeddedMetadataError::LimitExceeded)
    ));
    let attributes = (0..33).map(|i| format!(" a{i}='x'")).collect::<String>();
    assert!(matches!(
        EmbeddedMetadata::parse(format!("<ComicInfo{attributes}/>").as_bytes()),
        Err(EmbeddedMetadataError::LimitExceeded)
    ));
    for fields in [
        format!("<Title>{}</Title>", "x".repeat(4097)),
        format!("<Number>{}</Number>", "1".repeat(65)),
        format!("{}{}", "<Unknown>".repeat(16), "</Unknown>".repeat(16)),
        format!("<Web>{}</Web>", "https://example.org/ ".repeat(17)),
        format!("<Web>https://example.org/{}</Web>", "x".repeat(2048)),
    ] {
        let xml = format!("<ComicInfo>{fields}</ComicInfo>");
        assert!(matches!(
            EmbeddedMetadata::parse(xml.as_bytes()),
            Err(EmbeddedMetadataError::LimitExceeded)
        ));
    }
    assert_eq!(
        parse(&format!(
            "<ComicInfo><Title>{}</Title></ComicInfo>",
            "x".repeat(4096)
        ))
        .title
        .unwrap()
        .len(),
        4096
    );
    let at_limit = format!(
        "<ComicInfo>{}</ComicInfo>",
        " ".repeat(MAX_COMIC_INFO_BYTES - 23)
    );
    assert_eq!(at_limit.len(), MAX_COMIC_INFO_BYTES);
    assert!(EmbeddedMetadata::parse(at_limit.as_bytes()).is_ok());
}

#[test]
fn web_hints_exclude_active_schemes_and_credentials() {
    let hints = parse(
        "<ComicInfo><Web>javascript:alert(1) file:///tmp/data data:text/plain,hello https://user:pass@example.org/ //example.org/ https://example.org/</Web></ComicInfo>",
    );
    assert_eq!(hints.web_urls, ["https://example.org/"]);
    assert_eq!(
        parse("<ComicInfo><Title><![CDATA[A & B]]></Title></ComicInfo>")
            .title
            .as_deref(),
        Some("A & B")
    );
}

// Minimal stored ZIP generation for owned, synthetic test content; production uses only 7z.
fn archive(entries: &[(&str, &[u8])], encrypted: bool) -> (tempfile::TempDir, Arc<File>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("owned.cbz");
    let mut out = Vec::new();
    let mut central = Vec::new();
    for &(name, data) in entries {
        let offset = out.len() as u32;
        let mut crc = !0u32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        let crc = !crc;
        let flags = u16::from(encrypted);
        let size = data.len() as u32;
        out.extend(0x04034b50u32.to_le_bytes());
        for v in [20u16, flags, 0, 0, 0] {
            out.extend(v.to_le_bytes());
        }
        for v in [crc, size, size] {
            out.extend(v.to_le_bytes());
        }
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(name.as_bytes());
        out.extend(data);
        central.extend(0x02014b50u32.to_le_bytes());
        for v in [20u16, 20, flags, 0, 0, 0] {
            central.extend(v.to_le_bytes());
        }
        for v in [crc, size, size] {
            central.extend(v.to_le_bytes());
        }
        for v in [name.len() as u16, 0, 0, 0, 0] {
            central.extend(v.to_le_bytes());
        }
        central.extend(0u32.to_le_bytes());
        central.extend(offset.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let offset = out.len() as u32;
    let size = central.len() as u32;
    out.extend(central);
    out.extend(0x06054b50u32.to_le_bytes());
    for v in [0u16, 0, entries.len() as u16, entries.len() as u16] {
        out.extend(v.to_le_bytes());
    }
    out.extend(size.to_le_bytes());
    out.extend(offset.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    File::create(&path).unwrap().write_all(&out).unwrap();
    let file = Arc::new(File::open(path).unwrap());
    (dir, file)
}

#[tokio::test]
async fn reads_owned_cbz_from_retained_descriptor_after_unlink() {
    let (dir, file) = archive(
        &[(
            "folder/comicinfo.XML",
            b"<ComicInfo><Number>12.5</Number><LanguageISO>ja</LanguageISO></ComicInfo>",
        )],
        false,
    );
    std::fs::remove_file(dir.path().join("owned.cbz")).unwrap();
    let hints = EmbeddedMetadata::read(&ArchiveDecoder::new(), file)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hints.number.as_deref(), Some("12.5"));
    assert_eq!(hints.language_iso.as_deref(), Some("ja"));
}

#[tokio::test]
async fn missing_comic_info_is_not_an_error_or_fabricated_metadata() {
    let (_dir, file) = archive(&[("notes.txt", b"owned notes")], false);
    assert_eq!(
        EmbeddedMetadata::read(&ArchiveDecoder::new(), file)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn duplicate_unsafe_and_encrypted_archive_entries_are_rejected() {
    let xml = b"<ComicInfo/>".as_slice();
    let decoder = ArchiveDecoder::new();
    for entries in [
        vec![("ComicInfo.xml", xml), ("ComicInfo.xml", xml)],
        vec![("ComicInfo.xml", xml), ("folder/COMICINFO.XML", xml)],
        vec![("../ComicInfo.xml", xml)],
        vec![("/ComicInfo.xml", xml)],
        vec![("folder\\ComicInfo.xml", xml)],
        vec![("C:/ComicInfo.xml", xml)],
        vec![
            ("ComicInfo.xml", xml),
            ("../notes.txt", b"notes".as_slice()),
        ],
    ] {
        let (_dir, file) = archive(&entries, false);
        assert!(
            EmbeddedMetadata::read(&decoder, file).await.is_err(),
            "{entries:?}"
        );
    }
    let (_dir, file) = archive(&[("ComicInfo.xml", xml)], true);
    assert!(EmbeddedMetadata::read(&decoder, file).await.is_err());
}

#[tokio::test]
async fn archive_metadata_size_and_xml_are_checked() {
    let decoder = ArchiveDecoder::new();
    let large = vec![b' '; MAX_COMIC_INFO_BYTES + 1];
    let (_dir, file) = archive(&[("ComicInfo.xml", &large)], false);
    assert!(matches!(
        EmbeddedMetadata::read(&decoder, file).await,
        Err(EmbeddedMetadataError::Archive(ArchiveError::OutputTooLarge))
    ));
    let (_dir, file) = archive(
        &[("ComicInfo.xml", b"<!DOCTYPE ComicInfo><ComicInfo/>")],
        false,
    );
    assert!(matches!(
        EmbeddedMetadata::read(&decoder, file).await,
        Err(EmbeddedMetadataError::InvalidXml)
    ));
}

#[tokio::test]
async fn owned_rar_fixtures_have_no_hints_and_encrypted_rar_is_rejected() {
    let decoder = ArchiveDecoder::new();
    for name in ["rar5-valid.rar", "rar5-solid.rar"] {
        let file = Arc::new(File::open(format!("tests/fixtures/rar/{name}")).unwrap());
        assert_eq!(EmbeddedMetadata::read(&decoder, file).await.unwrap(), None);
    }
    for name in [
        // This fixture is readable but includes a symbolic link.
        "rar4-valid.rar",
        "rar4-encrypted.rar",
        "rar5-encrypted.rar",
        "rar5-truncated.rar",
    ] {
        let file = Arc::new(File::open(format!("tests/fixtures/rar/{name}")).unwrap());
        assert!(EmbeddedMetadata::read(&decoder, file).await.is_err());
    }
}
