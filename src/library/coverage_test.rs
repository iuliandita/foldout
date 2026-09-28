use super::{LibraryFormat, coverage::CoverageEvidence};

#[test]
fn coverage_requires_explicit_evidence() {
    assert!(CoverageEvidence::UserConfirmed.is_explicit());
    assert!(CoverageEvidence::TrustedMetadata.is_explicit());
    assert!(!CoverageEvidence::Filename.is_explicit());
}

#[test]
fn library_file_accepts_only_supported_formats() {
    assert_eq!("cbz".parse::<LibraryFormat>().unwrap(), LibraryFormat::Cbz);
    assert!("zip".parse::<LibraryFormat>().is_err());
}
