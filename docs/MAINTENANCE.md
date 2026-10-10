# Maintenance

This is the documentation review map for #48, not an automated drift guard.
Foldout remains unreleased; do not invent release or upgrade history.

## Source-to-documentation map

Paths below are relative to the repository root. Review the listed documentation
when changing the corresponding source; the table is not a second API inventory.

| Area | Source of truth | Documentation to review |
| --- | --- | --- |
| Configuration and CLI | `src/config.rs`, `src/cli.rs` | [README](../README.md) environment defaults and local commands; [operations](operations.md) startup, container configuration, and recovery commands |
| HTTP API and web types | `src/httpapi/`, `api/openapi.yaml`, `scripts/generate-api`, `web/src/lib/api/schema.d.ts` | [OpenAPI](../api/openapi.yaml) contract; [README](../README.md) API and generation instructions; [integrations](integrations.md) documented acquisition contracts |
| SQLite migrations | `migrations/`, `src/store/sqlite.rs` | [Operations](operations.md) restore, upgrade, and rollback guidance; [README](../README.md) schema compatibility summary |
| Backup and encryption | `src/backup/`, `src/settings/encryption.rs`, `tests/backup_restore.rs` | [Operations](operations.md) snapshot layout, key handling, validation, and recovery; [integrations](integrations.md) credential storage notes |
| Readers and formats | `src/reader/`, `src/httpapi/reader.rs`, `src/library/`, `src/importer/` | [Format validation](formats.md) supported formats, decoder requirements, limits, and evidence; [README](../README.md) reader and source-file behavior |
| Providers and download clients | `src/providers/`, `src/clients/`, `src/acquisition/` | [Integration status](integrations.md), [provider contracts](../src/providers/README.md), and [client contracts](../src/clients/README.md); [README](../README.md) capability summary |
| Rust build and web assets | `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `build.rs`, `src/webassets.rs`, `tools/package.json`, `tools/bun.lock`, `web/package.json`, `web/bun.lock`, `web/vite.config.ts`, `web/src/locales/en.ts`, `web/src/locales/es.ts`, `Dockerfile`, `compose.yaml` | [README](../README.md) build and development commands; [operations](operations.md) toolchain, container, runtime tools, and embedded assets |
| Acceptance gates | `scripts/check`, `.github/workflows/ci.yml` | [README](../README.md) check coverage; [operations](operations.md) verification instructions; this document's automation status |

## Change and release review

- For each affected area, update the relevant documentation in the PR or give a
  specific no-impact explanation citing the changed source and the documentation
  reviewed. A generic "docs unchanged" statement is not an assessment.
- Tie the assessment to the current PR diff. Cited documentation updates must
  actually change in that PR; an old assessment is not evidence for new changes.
- Check local links and documented environment names, defaults, accepted values,
  CLI arguments, runtime prerequisites, and examples against current source.
- Keep `api/openapi.yaml` aligned with HTTP behavior. Generate web API types with
  `scripts/generate-api`; do not hand-edit `web/src/lib/api/schema.d.ts` or maintain
  a competing endpoint list. `scripts/check` already compares generated types.
- Distinguish supported behavior, fixture tests, live checks, and planned work.
  Metadata lookup or connectivity is not proof of a successful transfer. Keep
  English and Spanish locale key sets equal when changing user-facing text.
- For a release review, assess the combined range since the previous release (or
  the development baseline before the first release). Later query-only changes
  do not erase earlier migration or backup compatibility obligations.

## Migration and backup compatibility

Add numbered migrations; never edit published migrations. Changes to schema,
backup manifests, encryption, or restore behavior require actionable notes in
[operations](operations.md): supported source schemas and backup formats, upgrade
steps, a verified pre-upgrade backup including its key, restore validation, and
rollback prerequisites. State incompatibilities and limitations explicitly.

Keep backup source state read-only and validate only disposable copies. Preserve
the original state and previous executable/image. Rollback restores a compatible
backup into fresh state; it does not run an older executable on a newer database.
There is no backward-schema or automatic-downgrade promise. Do not claim that a
documentation review proves an upgrade or restore works.

## Automated drift checks: future work

This change adds guidance only. Local-link/configuration checks, full PR-base and
combined-release-range obligation checks, and focused failure fixtures for stale
contracts, missing migration guidance, stale assessments, and missing tooling are
future work under #48. Any guard should reuse `scripts/check` and the existing API
contract, run deterministically offline in required PR/release gates, and fail
clearly when required tooling is absent. Optional hooks cannot replace those gates.
