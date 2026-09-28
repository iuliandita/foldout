use std::{ffi::OsString, path::PathBuf};

use crate::{backup, config::Config};

const HELP: &str = "libraryd [serve | backup DESTINATION | restore BACKUP FRESH_STATE | --version | --help]\n\nConfiguration: LIBRARY_STATE_DIR, LIBRARY_LISTEN, LIBRARY_ORIGIN.\nBackup creates a new private destination. Restore requires a new state directory.\nMedia files are not included in database backups.";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Serve,
    Backup(PathBuf),
    Restore(PathBuf, PathBuf),
    Help,
    Version,
}

fn parse(args: &[OsString]) -> Result<Command, &'static str> {
    match args {
        [] => Ok(Command::Serve),
        [command] if command == "serve" => Ok(Command::Serve),
        [command] if command == "--help" || command == "-h" => Ok(Command::Help),
        [command] if command == "--version" => Ok(Command::Version),
        [command, destination] if command == "backup" => Ok(Command::Backup(destination.into())),
        [command, backup, destination] if command == "restore" => {
            Ok(Command::Restore(backup.into(), destination.into()))
        }
        _ => Err("invalid command; run libraryd --help"),
    }
}

/// Returns true only when the caller should start the HTTP service.
pub async fn dispatch(args: Vec<OsString>) -> Result<bool, Box<dyn std::error::Error>> {
    match parse(&args)? {
        Command::Serve => return Ok(true),
        Command::Help => println!("{HELP}"),
        Command::Version => println!("libraryd {}", env!("CARGO_PKG_VERSION")),
        Command::Backup(destination) => {
            let config = Config::from_env()?;
            if !config.state_dir.join("library.sqlite3").is_file() {
                return Err("backup requires an existing state database".into());
            }
            backup::backup_existing(&config.state_dir, &destination).await?;
            println!(
                "{}",
                serde_json::json!({"status":"ok", "operation":"backup"})
            );
        }
        Command::Restore(source, destination) => {
            backup::restore(&source, &destination).await?;
            println!(
                "{}",
                serde_json::json!({"status":"ok", "operation":"restore"})
            );
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_or_incomplete_commands_never_start_the_service() {
        for values in [
            vec!["backup"],
            vec!["restore", "backup"],
            vec!["serve", "extra"],
            vec!["unknown"],
        ] {
            let args = values.into_iter().map(OsString::from).collect::<Vec<_>>();
            assert!(parse(&args).is_err());
        }
        assert_eq!(parse(&[]), Ok(Command::Serve));
        assert_eq!(
            parse(&["backup".into(), "destination with spaces".into()]),
            Ok(Command::Backup("destination with spaces".into()))
        );
    }
}
