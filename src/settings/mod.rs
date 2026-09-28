//! Administrator-owned integration configuration. Secrets never enter public DTOs.
mod adapters;
mod encryption;
mod model;

pub use adapters::IntegrationAdapter;
pub use encryption::EncryptionKey;
pub use model::*;

use crate::store::sqlite::SqliteStore;
use encryption::{Encrypted, Secrets};
use sqlx::Row;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Integration not found")]
    NotFound,
    #[error("Integration is disabled or lacks required credentials")]
    NotConfigured,
    #[error("Encryption key unavailable or insecure")]
    KeyUnavailable,
    #[error(
        "Stored integration credentials exist but encryption.key is missing; restore the original key"
    )]
    MissingKey,
    #[error("Credential authentication failed")]
    Encryption,
    #[error("Settings database operation failed")]
    Database,
}

impl From<sqlx::Error> for SettingsError {
    fn from(_: sqlx::Error) -> Self {
        Self::Database
    }
}

#[derive(Clone)]
pub struct Settings {
    store: SqliteStore,
    key: EncryptionKey,
}

/// Server-only material for adapters and workers. Never return from an HTTP handler.
pub(crate) struct PrivateIntegration {
    pub integration: Integration,
    pub api_key: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl Settings {
    pub fn new(store: SqliteStore, key: EncryptionKey) -> Self {
        Self { store, key }
    }

    pub async fn list(&self) -> Result<Vec<Integration>, SettingsError> {
        let rows = sqlx::query("SELECT * FROM integrations ORDER BY label, id")
            .fetch_all(self.store.reader())
            .await?;
        rows.iter()
            .map(|row| self.decode(row).map(|(view, _)| view))
            .collect()
    }

    pub async fn get(&self, id: &str) -> Result<Integration, SettingsError> {
        self.load(id).await.map(|(view, _)| view)
    }

    pub(crate) async fn load_private(&self, id: &str) -> Result<PrivateIntegration, SettingsError> {
        let (integration, secrets) = self.load(id).await?;
        Ok(PrivateIntegration {
            integration,
            api_key: secrets.api_key,
            username: secrets.username,
            password: secrets.password,
        })
    }

    pub async fn create(&self, input: CreateIntegration) -> Result<Integration, SettingsError> {
        model::validate_label_url(&input.label, &input.base_url)?;
        let options = IntegrationOptions::parse(input.kind, input.options)?;
        let mut secrets = Secrets::default();
        input.api_key.apply(&mut secrets.api_key)?;
        input.username.apply(&mut secrets.username)?;
        input.password.apply(&mut secrets.password)?;
        let view = view(
            uuid::Uuid::new_v4().to_string(),
            input.kind,
            input.label,
            input.base_url,
            input.enabled,
            options,
            &secrets,
        );
        let encrypted = self.encrypt(&view, &secrets)?;
        let options = serde_json::to_string(&view.options).map_err(|_| SettingsError::Database)?;
        let mut tx = self.store.begin_write().await?;
        sqlx::query("INSERT INTO integrations (id, kind, label, base_url, enabled, options, secret_version, secret_nonce, secret_ciphertext) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&view.id).bind(view.kind.as_str()).bind(&view.label).bind(&view.base_url).bind(view.enabled).bind(options)
            .bind(encrypted.version).bind(encrypted.nonce).bind(encrypted.ciphertext).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(view)
    }

    pub async fn update(
        &self,
        id: &str,
        input: UpdateIntegration,
    ) -> Result<Integration, SettingsError> {
        // Read and update on the same writer transaction: no stale secret overwrites.
        let mut tx = self.store.begin_write().await?;
        let row = sqlx::query("SELECT * FROM integrations WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(SettingsError::NotFound)?;
        let (old, mut secrets) = self.decode(&row)?;
        let label = input.label.unwrap_or(old.label);
        let base_url = input.base_url.unwrap_or(old.base_url);
        model::validate_label_url(&label, &base_url)?;
        let options = match input.options {
            Some(value) => IntegrationOptions::parse(old.kind, value)?,
            None => old.options,
        };
        input.api_key.apply(&mut secrets.api_key)?;
        input.username.apply(&mut secrets.username)?;
        input.password.apply(&mut secrets.password)?;
        let view = view(
            old.id,
            old.kind,
            label,
            base_url,
            input.enabled.unwrap_or(old.enabled),
            options,
            &secrets,
        );
        let encrypted = self.encrypt(&view, &secrets)?;
        let options = serde_json::to_string(&view.options).map_err(|_| SettingsError::Database)?;
        sqlx::query("UPDATE integrations SET label = ?, base_url = ?, enabled = ?, options = ?, secret_version = ?, secret_nonce = ?, secret_ciphertext = ? WHERE id = ?")
            .bind(&view.label).bind(&view.base_url).bind(view.enabled).bind(options)
            .bind(encrypted.version).bind(encrypted.nonce).bind(encrypted.ciphertext).bind(&view.id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(view)
    }

    pub async fn delete(&self, id: &str) -> Result<(), SettingsError> {
        let mut tx = self.store.begin_write().await?;
        let result = sqlx::query("DELETE FROM integrations WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if result.rows_affected() == 0 {
            return Err(SettingsError::NotFound);
        }
        tx.commit().await?;
        Ok(())
    }

    async fn load(&self, id: &str) -> Result<(Integration, Secrets), SettingsError> {
        let row = sqlx::query("SELECT * FROM integrations WHERE id = ?")
            .bind(id)
            .fetch_optional(self.store.reader())
            .await?
            .ok_or(SettingsError::NotFound)?;
        self.decode(&row)
    }

    fn encrypt(&self, view: &Integration, secrets: &Secrets) -> Result<Encrypted, SettingsError> {
        let mut plaintext = secrets.encode();
        let result = self.key.encrypt(&view.id, view.kind.as_str(), &plaintext);
        plaintext.fill(0);
        result
    }

    fn decode(
        &self,
        row: &sqlx::sqlite::SqliteRow,
    ) -> Result<(Integration, Secrets), SettingsError> {
        let id: String = row.try_get("id")?;
        let kind: String = row.try_get("kind")?;
        let encrypted = Encrypted {
            version: row.try_get("secret_version")?,
            nonce: row.try_get("secret_nonce")?,
            ciphertext: row.try_get("secret_ciphertext")?,
        };
        let mut plaintext = self.key.decrypt(&id, &kind, &encrypted)?;
        let secrets = Secrets::decode(&plaintext);
        plaintext.fill(0);
        let secrets = secrets?;
        let kind: IntegrationKind = serde_json::from_value(serde_json::Value::String(kind))
            .map_err(|_| SettingsError::Database)?;
        let options: String = row.try_get("options")?;
        let options = IntegrationOptions::parse(
            kind,
            serde_json::from_str(&options).map_err(|_| SettingsError::Database)?,
        )
        .map_err(|_| SettingsError::Database)?;
        let label: String = row.try_get("label")?;
        let base_url: String = row.try_get("base_url")?;
        model::validate_label_url(&label, &base_url).map_err(|_| SettingsError::Database)?;
        Ok((
            view(
                id,
                kind,
                label,
                base_url,
                row.try_get("enabled")?,
                options,
                &secrets,
            ),
            secrets,
        ))
    }
}

fn view(
    id: String,
    kind: IntegrationKind,
    label: String,
    base_url: String,
    enabled: bool,
    options: IntegrationOptions,
    secrets: &Secrets,
) -> Integration {
    let api_key_configured = secrets.api_key.is_some();
    let username_configured = secrets.username.is_some();
    let password_configured = secrets.password.is_some();
    let credentials_configured = match kind {
        IntegrationKind::ComicVine | IntegrationKind::Prowlarr | IntegrationKind::Sabnzbd => {
            api_key_configured
        }
        IntegrationKind::QBittorrent => username_configured && password_configured,
        _ => true,
    };
    Integration {
        id,
        kind,
        label,
        base_url,
        enabled,
        options,
        api_key_configured,
        username_configured,
        password_configured,
        credentials_configured,
    }
}

#[cfg(test)]
mod tests;
