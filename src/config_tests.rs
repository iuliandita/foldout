use std::net::SocketAddr;
use std::path::PathBuf;

use crate::config::Config;

#[test]
fn worker_switch_rejects_ambiguous_values() {
    assert!(!super::parse_workers("false").unwrap());
    assert!(super::parse_workers("true").unwrap());
    for value in ["", "0", "False", "disabled"] {
        assert!(super::parse_workers(value).is_err());
    }
}

#[test]
fn origin_validation_rejects_credentials_paths_and_non_http_schemes() {
    for value in [
        "https://user:secret@example.test",
        "https://example.test/path",
        "file:///tmp/file",
        "https://example.test?token=x",
        "https://example.test#fragment",
        "null",
    ] {
        assert!(Config::validate_origin(value).is_err(), "{value}");
    }
    assert_eq!(
        Config::validate_origin("https://library.example/").unwrap(),
        "https://library.example"
    );
}

#[test]
fn parse_accepts_explicit_state_directory_and_listen_address() {
    let config = Config::parse(PathBuf::from("state"), "127.0.0.1:9000").unwrap();

    assert_eq!(config.state_dir, PathBuf::from("state"));
    assert_eq!(
        config.listen,
        "127.0.0.1:9000".parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn parse_rejects_an_empty_state_directory() {
    let error = Config::parse(PathBuf::new(), "127.0.0.1:9000").unwrap_err();

    assert!(error.to_string().contains("state directory"));
}

#[test]
fn parse_rejects_an_invalid_listen_address() {
    let error = Config::parse(PathBuf::from("state"), "invalid").unwrap_err();

    assert!(error.to_string().contains("listen address"));
}
