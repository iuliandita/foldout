# Documentation maintenance

This is the source-to-documentation review map for [#48](https://github.com/iuliandita/foldout/issues/48), not an automated drift guard. Foldout remains unreleased; do not invent release history or describe planned capabilities as supported behavior.

## Source map

Paths below are relative to the repository root. Use the implementation and existing contracts rather than maintaining a second API inventory.

| Change area | Sources to inspect | Documentation to assess |
| --- | --- | --- |
| Configuration and CLI | `src/config.rs`, `src/config_tests.rs`, `src/cli.rs`, `src/main.rs` | [README](../README.md) environment defaults, local startup and commands; [operations](operations.md) container configuration and recovery commands |
| HTTP contract and generated web types | `src/httpapi/`, `api/openapi.yaml`, `scripts/generate-api`, `web/src/lib/api/schema.d.ts` | [OpenAPI](../api/openapi.yaml), then affected examples and behavior in [README](../README.md), [operations](operations.md) and [integrations](integrations.md) |
| SQLite migrations | `migrations/`, `src/store/sqlite.rs`, `src/store/sqlite_test.rs` | [operations](operations.md) startup, upgrade, restore and rollback guidance; [README](../README.md) compatibility summary |
| Backup and encryption | `src/backup/mod.rs`, `src/backup/files.rs`, `src/settings/encryption.rs`, `tests/backup_restore.rs` | [operations](operations.md) snapshot layout, key handling, permissions, validation and restore drill; [integrations](integrations.md) credential storage; [README](../README.md) backup commands |
| Readers and integrations | `src/reader/`, `src/providers/`, `src/clients/`, `src/acquisition/`, `src/search/`, `src/importer/` | [formats](formats.md) supported formats and limits; [integrations](integrations.md) adapter behavior, selection/submission fences and evidence; [README](../README.md) feature summary |
| Rust build and web assets | `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `build.rs`, `src/webassets.rs`, `tools/package.json`, `tools/bun.lock`, `web/package.json`, `web/bun.lock`, `web/vite.config.ts`, `web/src/`, `scripts/bun`, `scripts/check`, `Dockerfile`, `compose.yaml`, `.github/workflows/ci.yml`, `.github/workflows/nightly.yml` | [README](../README.md) prerequisites, build/check commands and embedded UI; [operations](operations.md) tool versions, runtime dependencies, container and nightly status |

## Review obligations

- For each affected area, update the relevant documentation in the same PR or give a specific no-impact explanation citing the changed source and the documentation assessed. A generic “docs not needed” is not enough. Tie the assessment to the current revision and refresh it after changes; an update claim must cite documentation actually changed in the PR.
- Review the full PR-base diff, including new files. For a release, assess the combined release range: a later query-only change does not cancel an earlier migration or backup-format obligation.
- Check local documentation links and verify documented configuration names, defaults, accepted values and CLI arguments against the current source. Keep examples consistent with the contract; never hand-edit generated API types. After changing `api/openapi.yaml`, regenerate them through `./scripts/generate-api` and use `./scripts/check` for the existing contract comparison.
- Distinguish supported formats, local protocol tests, disposable transfer tests and live evidence from planned or unverified behavior. Metadata lookup or connectivity is not proof of acquisition readiness. Preserve the release-selection and attempted-submission fences in descriptions as well as code.
- Assess visible UI behavior and asset/build requirements, not just Rust changes. Keep the English and Spanish locale key sets equal when changing UI text.

These are review obligations today, not claims that CI validates the prose.

## Migration and backup compatibility

Never edit published migrations; add numbered migrations. For schema, backup-format or encryption changes, update [operations](operations.md) with actionable guidance covering:

1. **Compatibility:** Identify affected schema/format versions, which existing state and backups the new executable accepts, breaking behavior and unsupported combinations. State limits explicitly rather than promising backward compatibility.
2. **Backup:** Retain a verified recovery point and the previous executable/image before upgrading. Include the saved encryption key where required; media and environment configuration need separate backups. Keep backups private.
3. **Upgrade:** Explain prerequisites, the migration path, expected effects and validation steps using disposable copied state. Backup sources and published snapshots stay unchanged; only disposable validation or fresh restored state may be migrated.
4. **Rollback:** Describe recovery into a fresh directory with a compatible executable and backup. Do not suggest opening a newly migrated database with an older executable or restoring over active state. There is no automated downgrade promise.

Use the existing [restore drill](operations.md#restore-drill-and-upgrade) and `tests/backup_restore.rs` as the baseline. Report what was actually verified and what remains untested; a passing unit test does not prove a live upgrade or restore.

## Checks today and future work

`./scripts/check` already runs offline after dependencies are installed. It checks Rust formatting, Clippy and tests, compares regenerated OpenAPI web types, checks TypeScript, builds web assets, tests the embedded UI and builds the release executable. It is not a documentation drift checker.

Future work under #48, not implemented by this document:

- Add deterministic offline local-link and documented-configuration checks to required PR/release gates, reusing `api/openapi.yaml`, `scripts/generate-api` and `scripts/check` rather than introducing a competing contract system.
- Assess the full PR-base diff with enough history and the combined release range. Include new source files, test exclusions, and fail clearly when required tooling is missing. Local hooks can supplement, not replace, required CI.
- Tie documentation updates or justified no-impact assessments to current source; reject stale assessments and baseline resets that bypass review. Require compatibility guidance for migrations and backup-format changes.
- Add focused failure fixtures for real drift, stale contracts/assessments, missing tooling and migration guidance, and combined release ranges. Keep checks offline and independent of model services.

Automation can enforce review obligations and verifiable facts. It cannot prove prose correctness, live integration behavior or successful recovery. #48 remains open for that follow-up work.
