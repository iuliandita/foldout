# Documentation maintenance

Review the listed documents when changing a source. Update affected guidance or explain the specific lack of impact in the pull request. This table routes review; it is not an inventory of API behavior.

| Area | Sources | Review docs |
| --- | --- | --- |
| Configuration | `src/config.rs`, `src/main.rs`, `build.rs` | `README.md`, `docs/operations.md` |
| CLI | `src/cli.rs` | `README.md`, `docs/operations.md` |
| API | `api/openapi.yaml`, `web/src/lib/api/schema.d.ts`, `scripts/generate-api`, `scripts/check` | `README.md`, `CONTRIBUTING.md` |
| Database | `migrations`, `src/store` | `docs/operations.md` |
| Backup and encryption | `src/backup`, `src/settings/encryption.rs` | `README.md`, `docs/operations.md` |
| Reader | `src/reader` | `README.md`, `docs/formats.md` |
| Integrations | `src/clients`, `src/providers`, `src/acquisition` | `README.md`, `docs/integrations.md` |
| Build and web assets | `Cargo.toml`, `Dockerfile`, `web/src`, `src/webassets.rs` | `README.md`, `docs/operations.md` |

For migration or backup-format changes, add actionable upgrade, backup, rollback, and compatibility notes, and validate them with isolated state. Never edit a published migration.

The API contract is `api/openapi.yaml`; `scripts/generate-api` generates the web types and `scripts/check` verifies them. The checks below validate paths, local links, and these narrowly scoped documented source claims, not prose accuracy or live recovery.

## Checked source claims

- Environment in `src/config.rs`, documented in `README.md`: `LIBRARY_STATE_DIR`, `LIBRARY_LISTEN`, `LIBRARY_ORIGIN`, `LIBRARY_WORKERS`.
- Commands in `src/cli.rs`, documented in `README.md`: `backup`, `restore`.

The pull-request diff requirement remains pending in [#48](https://github.com/iuliandita/foldout/issues/48).
