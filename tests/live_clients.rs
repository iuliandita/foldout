use libraryd::clients::{ClientConfig, HttpLimits, QBittorrent, Sabnzbd};
use serde::Deserialize;
#[derive(Deserialize)]
struct Config {
    kind: String,
    base_url: String,
    api_key: Option<String>,
    username: Option<String>,
    password: Option<String>,
}
#[tokio::test]
#[ignore = "requires explicit read-only live-service configuration"]
async fn live_download_client_versions() {
    let path =
        std::env::var_os("LIBRARY_LIVE_CLIENTS_CONFIG_FILE").expect("set configuration file path");
    let input = std::fs::read(path).expect("read protected live-test config");
    let configs: Vec<Config> = serde_json::from_slice(&input).expect("parse live-test config");
    assert!(!configs.is_empty());
    for config in configs {
        let client = ClientConfig::new(
            uuid::Uuid::new_v4(),
            &config.base_url,
            "library-test".into(),
            HttpLimits::default(),
        )
        .unwrap();
        let info = match config.kind.as_str() {
            "sabnzbd" => Sabnzbd::new(client, config.api_key.expect("SAB key required"))
                .unwrap()
                .test_connection()
                .await
                .unwrap(),
            "qbittorrent" => QBittorrent::new(
                client,
                config.username.expect("username required"),
                config.password.expect("password required"),
            )
            .unwrap()
            .test_connection()
            .await
            .unwrap(),
            _ => panic!("unsupported configured client kind"),
        };
        println!(
            "{} authenticated adapter version {}",
            config.kind, info.version
        );
    }
}
