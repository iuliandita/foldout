# Documentation maintenance

This source map is the first documentation increment for [issue #48](https://github.com/iuliandita/foldout/issues/48), not an automated drift guard.
Foldout remains unreleased; do not invent release history or present planned capabilities as supported behavior.

## Source-to-documentation map

Paths are relative to the repository root. Review all affected rows; this is not a second API inventory.

| Area | Source of behavior or contract | Documentation to review |
| --- | --- | --- |
| Rust configuration and CLI | `src/config.rs`, `src/cli.rs` | [README](../README.md) configuration/defaults and commands; [operations](operations.md) startup and recovery |
| HTTP API and generated web types | `src/httpapi/`, [OpenAPI](../api/openapi.yaml), `scripts/generate-api`, `web/src/lib/api/schema.d.ts`, `web/src/lib/api/client.ts` | OpenAPI contract; README API guidance; relevant operations/integration instructions |
| SQLite migrations | `migrations/`, `src/store/sqlite.rs` | Operations upgrade, restore, rollback, and schema compatibility guidance |
| Backup and encryption | `src/backup/mod.rs`, `src/backup/files.rs`, `src/settings/encryption.rs`, `tests/backup_restore.rs` | Operations backup layout, key handling, validation, restore, and compatibility; [integration status](integrations.md) credential handling |
| Readers and formats | `src/reader/archive.rs`, `src/reader/pdf.rs`, `src/reader/service.rs`, `tests/reader_http.rs`, `tests/fixtures/manifest.json` | [Format validation](formats.md) support, limits, and fixture evidence; operations runtime tools |
| Providers, clients, and acquisition/import | `src/providers/`, `src/clients/`, `src/acquisition/`, `src/importer/`, `tests/providers_http.rs`, `tests/clients_http.rs` | Integration status; [provider contracts](../src/providers/README.md); [client contracts](../src/clients/README.md); README workflow guidance |
| Rust build and packaging | `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `build.rs`, `src/webassets.rs`, `Dockerfile`, `compose.yaml`, `tests/build_boundary.rs` | README build commands and embedded UI requirements; operations container/toolchain instructions |
| Web assets and tools | `web/src/`, `web/package.json`, `web/bun.lock`, `web/vite.config.ts`, `tools/package.json`, `tools/bun.lock`, `scripts/bun` | README development/build instructions; operations asset packaging; user-facing behavior in the relevant docs |
| Acceptance gates | `scripts/check`, `.github/workflows/ci.yml` | README check description; operations verification instructions |

## Change and review obligations

- For each affected area, update the relevant documentation in the same PR or give a specific no-impact explanation citing the changed source and why documented behavior remains unchanged. A generic “internal change” is insufficient.
- Tie the assessment to the current full PR-base diff, including new files. Reassess after source changes; for a release, review the combined release range so a later change cannot erase an earlier migration or backup obligation.
- Check local documentation links and documented configuration names, defaults, validation, and CLI arguments against current source. Cited documentation updates must actually be present in the PR, not just in an older assessment.
- Keep `api/openapi.yaml` as the API contract and generate web types with `scripts/generate-api`; do not hand-edit generated types or maintain a competing endpoint list. `scripts/check` already compares freshly generated types with the committed schema.
- Distinguish format support, fixture evidence, tested integration behavior, and remaining live-transfer work. Connectivity or metadata lookup is not proof of acquisition readiness. Keep English and Spanish locale key sets equal when changing UI text.

## Migration and backup compatibility

- Add numbered migrations; never edit published migrations. For migration, backup-format, or encryption changes, document affected schema/format versions, supported upgrade and restore paths, key requirements, incompatible cases, and actionable backup and rollback steps in operations.
- Retain a verified backup and the previous executable/image before upgrading. Restore into fresh state for rollback; do not run an older executable against a database migrated by a newer one. There is no backward-schema or automated-downgrade promise.
- Backup source state and published snapshots stay unchanged; validate and migrate only disposable copies. Backups include the database and saved encryption key, not media or environment configuration. Compatibility evidence must cover credential recovery as well as database integrity.
- Record what was actually verified using disposable state, with explicit limits. Documentation review cannot establish that a live upgrade or restore works.

## Existing checks and future work

`scripts/check` is the existing offline acceptance gate after dependency installation: Rust formatting, Clippy and tests, generated API comparison, TypeScript, frontend build, embedded-asset checks, and release build. CI also has a Docker build gate.

Automated source-impact classification, documentation/no-impact assessment validation, local-link and configuration drift checks, migration-guidance failure fixtures, and combined release-range checks are **future work under #48**. This increment adds no enforcement, hooks, dependencies, or CI changes. Required checks, independent review, and owner approval still apply; neither this map nor a no-impact explanation substitutes for them.
