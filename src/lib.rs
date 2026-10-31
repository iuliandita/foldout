#[cfg(all(feature = "soak-fixtures", not(debug_assertions)))]
compile_error!("soak-fixtures is restricted to disposable debug builds");
#[cfg(all(feature = "soak-fixtures", feature = "embedded-ui"))]
compile_error!("soak-fixtures cannot be combined with shipped embedded-ui artifacts");

/// Package version and immutable build revision, shared by the CLI and About API.
pub const VERSION: &str = if cfg!(feature = "soak-fixtures") {
    concat!(
        env!("CARGO_PKG_VERSION"),
        " (",
        env!("LIBRARY_BUILD_COMMIT"),
        ") [soak-fixtures debug]"
    )
} else {
    concat!(
        env!("CARGO_PKG_VERSION"),
        " (",
        env!("LIBRARY_BUILD_COMMIT"),
        ")"
    )
};

pub mod acquisition;
pub mod app;
pub mod auth;
pub mod backup;
pub mod bootstrap;
pub mod catalog;
pub mod cli;
pub mod clients;
pub mod config;
pub mod httpapi;
pub mod importer;
pub mod jobs;
pub mod library;
pub mod providers;
pub mod reader;
pub mod search;
pub mod settings;
pub mod store;
pub mod webassets;
pub mod worker;
