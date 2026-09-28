use std::path::Path;

use sha2::{Digest, Sha256};

use super::archive::{ArchiveDecoder, ArchiveError, parse_manifest};

const NATURAL_ORDER_CBZ: &str = "tests/fixtures/natural-order.cbz";

#[test]
fn listing_orders_numeric_page_names_and_totals_sizes() {
    let manifest = parse_manifest(
        b"Path = 10.png\nSize = 30\nEncrypted = -\n\nPath = 2.jpg\nSize = 20\nEncrypted = -\n\nPath = 1.webp\nSize = 10\nEncrypted = -\n",
    )
    .unwrap();

    assert_eq!(manifest.total_bytes, 60);
    assert_eq!(
        manifest
            .pages
            .into_iter()
            .map(|page| page.name)
            .collect::<Vec<_>>(),
        ["1.webp", "2.jpg", "10.png"]
    );
}

#[test]
fn listing_skips_non_image_records_but_rejects_unsafe_images() {
    let non_image = parse_manifest(b"Path = notes.txt\nSize = 2\nEncrypted = -\n");
    assert!(matches!(non_image, Err(ArchiveError::NoPages)));

    for name in [
        "../page.png",
        "/page.png",
        "folder/../page.png",
        "page\x01.png",
    ] {
        let listing = format!("Path = {name}\nSize = 1\nEncrypted = -\n");
        assert!(matches!(
            parse_manifest(listing.as_bytes()),
            Err(ArchiveError::InvalidListing(_))
        ));
    }
}

#[test]
fn listing_rejects_duplicate_encrypted_and_linked_pages() {
    let duplicate =
        b"Path = page.png\nSize = 1\nEncrypted = -\n\nPath = page.png\nSize = 1\nEncrypted = -\n";
    assert!(matches!(
        parse_manifest(duplicate),
        Err(ArchiveError::InvalidListing(_))
    ));

    let encrypted = b"Path = page.png\nSize = 1\nEncrypted = +\n";
    assert!(matches!(
        parse_manifest(encrypted),
        Err(ArchiveError::InvalidListing(_))
    ));

    let linked = b"Path = page.png\nSize = 1\nEncrypted = -\nSymbolic Link = other.png\n";
    assert!(matches!(
        parse_manifest(linked),
        Err(ArchiveError::InvalidListing(_))
    ));
}

#[test]
fn listing_rejects_page_size_bound() {
    let listing = b"Path = page.png\nSize = 33554433\nEncrypted = -\n";
    assert!(matches!(
        parse_manifest(listing),
        Err(ArchiveError::OutputTooLarge)
    ));
}

#[tokio::test]
async fn decoder_reads_owned_cbz_in_natural_order_without_mutating_pages() {
    let decoder = ArchiveDecoder::new();
    let archive = Path::new(NATURAL_ORDER_CBZ);

    let manifest = decoder.manifest(archive).await.unwrap();
    assert_eq!(manifest.total_bytes, 258);
    assert_eq!(
        manifest
            .pages
            .iter()
            .map(|page| page.name.as_str())
            .collect::<Vec<_>>(),
        ["1.png", "2.png", "10.png"]
    );

    let page = decoder.page(archive, "1.png").await.unwrap();
    assert_eq!(page.content_type, "image/png");
    assert_eq!((page.width, page.height), (16, 24));
    let hash = Sha256::digest(&page.bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(
        hash,
        "87a5833677bd5fad1be632dccac641b973fe27a576be139da62023df0202e300"
    );
}
