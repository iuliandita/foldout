# Documentation maintenance

This source-to-documentation map is the bounded documentation increment for [issue #48](https://github.com/iuliandita/foldout/issues/48). It records review obligations, not an automated documentation-drift gate. Foldout remains unreleased; do not infer a release history from development migrations or package versions.

## Source map

| Change area | Source to inspect | Documentation to assess |
| --- | --- | --- |
| Rust configuration and CLI | [src/config.rs](../src/config.rs), [src/cli.rs](../src/cli.rs) | [README](../README.md) environment defaults and local commands; [operations](operations.md) startup and recovery commands |
| HTTP API and generated web types | [src/httpapi](../src/httpapi), [api/openapi.yaml](../api/openapi.yaml), [scripts/generate-api](../scripts/generate-api), [web/src/lib/api/schema.d.ts](../web/src/lib/api/schema.d.ts) | Update the existing OpenAPI contract and regenerate web types; assess API usage in [README](../README.md) and [integrations](integrations.md). Do not add a second API inventory or hand-edit generated types. |
| SQLite schema and startup compatibility | [migrations](../migrations), [src/store/sqlite.rs](../src/store/sqlite.rs) | [Operations](operations.md) restore drill, upgrade, and rollback guidance; [README](../README.md) schema compatibility summary |
| Backup format and settings encryption | [src/backup/mod.rs](../src/backup/mod.rs), [src/settings/encryption.rs](../src/settings/encryption.rs), [tests/backup_restore.rs](../tests/backup_restore.rs) | [Operations](operations.md) snapshot layout, key handling, validation, and recovery; [integrations](integrations.md) credential-storage notes |
| Reader formats and decoder limits | [src/reader](../src/reader), [tests/reader_http.rs](../tests/reader_http.rs) | [Formats](formats.md) supported formats, limits, fixture evidence, and runtime tools; [README](../README.md) reader summary |
| Providers, clients, and acquisition | [src/providers](../src/providers), [src/clients](../src/clients), [src/acquisition](../src/acquisition) | [Integrations](integrations.md) implemented behavior, submission fences, tested evidence, and remaining work; [formats](formats.md) acquisition format limits |
| Rust build and container packaging | [Cargo.toml](../Cargo.toml), [Cargo.lock](../Cargo.lock), [rust-toolchain.toml](../rust-toolchain.toml), [build.rs](../build.rs), [Dockerfile](../Dockerfile), [compose.yaml](../compose.yaml) | [README](../README.md) build prerequisites and embedded UI; [operations](operations.md) container tools, storage, and upgrade instructions |
| Web assets and build checks | [web/package.json](../web/package.json), [web/vite.config.ts](../web/vite.config.ts), [src/webassets.rs](../src/webassets.rs), [tools/package.json](../tools/package.json), [scripts/check](../scripts/check) | [README](../README.md) frontend development and check commands; [operations](operations.md) compiled assets and runtime requirements. Keep [English](../web/src/locales/en.ts) and [Spanish](../web/src/locales/es.ts) locale key sets equal. |

## Change-review obligations

For each affected area, update the relevant documentation in the PR or give a specific no-impact explanation tied to the current source changes and cited documentation. A generic “docs unchanged” statement is not sufficient. Review the full PR-base diff; for release preparation, review the combined release range so later changes do not hide earlier migration or backup obligations.

Check local links and compare documented configuration names, defaults, CLI arguments, and build prerequisites against their sources. Distinguish supported behavior, fixture-tested behavior, live evidence, and planned work. A protocol or metadata test is not evidence of a successful transfer. Documentation changes cannot establish that an upgrade or restore works.

## Migration and backup compatibility

Never edit published migrations; add a numbered migration. For schema, backup-format, or encryption changes, document affected versions or schemas, supported restore inputs, prerequisites, upgrade steps, a verified-backup requirement, and an actionable rollback path in [operations](operations.md). Explain incompatibilities and unsupported downgrade paths explicitly. If an obligation does not apply, give a source-specific reason rather than omitting it.

Keep backup source state read-only and validate only disposable copies. Preserve the database and its saved encryption key together; media and environment configuration need separate recovery arrangements. Before an upgrade, retain a verified backup and the previous executable/image. Roll back by restoring compatible state into a fresh directory, not by running an older executable against newly migrated state. There is no backward-schema compatibility or automatic-downgrade promise.

## Existing checks and future work

[scripts/check](../scripts/check) remains the existing offline acceptance gate after dependency installation. It checks Rust formatting, Clippy and tests, regenerates and compares API types, checks and builds the frontend, and exercises embedded assets and the release build. It does not currently enforce the source-to-documentation obligations above.

Automated source-impact assessment, local-link and documented-configuration validation, full PR/release-range enforcement, and focused failure fixtures for stale contracts or missing migration guidance are future work under #48, not implemented by this document. Reuse the existing contract and deterministic checks rather than introducing a competing inventory. Such checks can enforce review obligations and verifiable facts, not prove prose accuracy or live upgrade/restore safety.
