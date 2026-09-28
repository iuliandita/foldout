use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};

use super::SettingsError;

/// Load before opening a DB write transaction. Neither key nor secrets are printable.
#[derive(Clone)]
pub struct EncryptionKey(Arc<XChaCha20Poly1305>);

pub(super) struct Encrypted {
    pub version: i64,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

impl EncryptionKey {
    /// The state directory must be private (0700). Existing permissions are never changed.
    /// Run during startup, before handing the key to Settings or acquiring a DB writer.
    pub async fn load_or_create(state_dir: &Path) -> Result<Self, SettingsError> {
        let directory = state_dir.to_owned();
        let encrypted_rows = encrypted_rows(state_dir).await?;
        tokio::task::spawn_blocking(move || Self::load_sync(&directory, encrypted_rows))
            .await
            .map_err(|_| SettingsError::KeyUnavailable)?
    }

    #[cfg(unix)]
    fn load_sync(directory: &Path, encrypted_rows: bool) -> Result<Self, SettingsError> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
        let io_error = |_| SettingsError::KeyUnavailable;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .map_err(io_error)?;
        let metadata = fs::symlink_metadata(directory).map_err(io_error)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.mode() & 0o077 != 0 {
            return Err(SettingsError::KeyUnavailable);
        }
        let path = directory.join("encryption.key");
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if encrypted_rows {
                    return Err(SettingsError::MissingKey);
                }
                let temporary = directory.join(format!(".encryption-{}.tmp", uuid::Uuid::new_v4()));
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&temporary)
                    .map_err(io_error)?;
                let cleanup = TemporaryKey(temporary);
                let mut bytes = [0u8; 32];
                getrandom::fill(&mut bytes).map_err(|_| SettingsError::KeyUnavailable)?;
                file.write_all(&bytes).map_err(io_error)?;
                bytes.fill(0);
                file.sync_all().map_err(io_error)?;
                // Linking a fully synced inode is atomic and never overwrites a competing key.
                match fs::hard_link(&cleanup.0, &path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => return Err(SettingsError::KeyUnavailable),
                }
                fs::remove_file(&cleanup.0).map_err(io_error)?;
            }
            Err(_) => return Err(SettingsError::KeyUnavailable),
        }
        let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.mode() & 0o777 != 0o600
            || metadata.len() != 32
            || metadata.uid() != fs::metadata(directory).map_err(io_error)?.uid()
        {
            return Err(SettingsError::KeyUnavailable);
        }
        let mut file = File::open(&path).map_err(io_error)?;
        let opened = file.metadata().map_err(io_error)?;
        if opened.ino() != metadata.ino() || opened.dev() != metadata.dev() {
            return Err(SettingsError::KeyUnavailable);
        }
        let mut bytes = [0u8; 32];
        file.read_exact(&mut bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        File::open(directory)
            .and_then(|d| d.sync_all())
            .map_err(io_error)?;
        let cipher = XChaCha20Poly1305::new((&bytes).into());
        bytes.fill(0);
        Ok(Self(Arc::new(cipher)))
    }

    #[cfg(not(unix))]
    fn load_sync(_: &Path, _: bool) -> Result<Self, SettingsError> {
        // Do not claim POSIX key confidentiality on platforms without this permission model.
        Err(SettingsError::KeyUnavailable)
    }

    pub(super) fn encrypt(
        &self,
        id: &str,
        kind: &str,
        plaintext: &[u8],
    ) -> Result<Encrypted, SettingsError> {
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).map_err(|_| SettingsError::Encryption)?;
        let aad = aad(id, kind);
        let ciphertext = self
            .0
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| SettingsError::Encryption)?;
        Ok(Encrypted {
            version: 1,
            nonce: nonce.to_vec(),
            ciphertext,
        })
    }

    pub(super) fn decrypt(
        &self,
        id: &str,
        kind: &str,
        encrypted: &Encrypted,
    ) -> Result<Vec<u8>, SettingsError> {
        let nonce: [u8; 24] = encrypted
            .nonce
            .as_slice()
            .try_into()
            .map_err(|_| SettingsError::Encryption)?;
        if encrypted.version != 1 || encrypted.ciphertext.len() > 12316 {
            return Err(SettingsError::Encryption);
        }
        let aad = aad(id, kind);
        self.0
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &encrypted.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| SettingsError::Encryption)
    }
}

// A separate read-only connection keeps key startup independent of the store's single writer.
async fn encrypted_rows(directory: &Path) -> Result<bool, SettingsError> {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let database = directory.join("library.sqlite3");
    match tokio::fs::symlink_metadata(&database).await {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err(SettingsError::Database),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(SettingsError::Database),
    }
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(database)
            .read_only(true),
    )
    .await?;
    let table_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'integrations')",
    )
    .fetch_one(&mut connection)
    .await?;
    let exists = if table_exists {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM integrations)")
            .fetch_one(&mut connection)
            .await?
    } else {
        false
    };
    connection.close().await?;
    Ok(exists)
}

fn aad(id: &str, kind: &str) -> Vec<u8> {
    format!("libraryd/integration-credentials/v1\0{id}\0{kind}").into_bytes()
}

struct TemporaryKey(PathBuf);
impl Drop for TemporaryKey {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[derive(Default)]
pub(super) struct Secrets {
    pub api_key: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl Secrets {
    // Fixed binary layout avoids giving credentials a Serialize implementation.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for field in [&self.api_key, &self.username, &self.password] {
            match field {
                Some(value) => {
                    bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
                    bytes.extend_from_slice(value.as_bytes());
                }
                None => bytes.extend_from_slice(&u32::MAX.to_be_bytes()),
            }
        }
        bytes
    }

    pub fn decode(mut bytes: &[u8]) -> Result<Self, SettingsError> {
        let mut fields = [None, None, None];
        for field in &mut fields {
            if bytes.len() < 4 {
                return Err(SettingsError::Encryption);
            }
            let length = u32::from_be_bytes(
                bytes[..4]
                    .try_into()
                    .map_err(|_| SettingsError::Encryption)?,
            );
            bytes = &bytes[4..];
            if length == u32::MAX {
                continue;
            }
            let length = length as usize;
            if length == 0 || length > 4096 || bytes.len() < length {
                return Err(SettingsError::Encryption);
            }
            *field = Some(
                std::str::from_utf8(&bytes[..length])
                    .map_err(|_| SettingsError::Encryption)?
                    .to_owned(),
            );
            bytes = &bytes[length..];
        }
        if !bytes.is_empty() {
            return Err(SettingsError::Encryption);
        }
        let [api_key, username, password] = fields;
        Ok(Self {
            api_key,
            username,
            password,
        })
    }
}
