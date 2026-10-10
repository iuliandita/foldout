# Documentation maintenance

This source map establishes manual documentation review obligations for [#48](https://github.com/iuliandita/foldout/issues/48). Automated drift checks remain future work; this document does not implement a gate. Foldout is unreleased, with no tagged release or released upgrade history to document.

## Source-to-documentation map

Paths below are relative to the repository root. Review every applicable row, including overlapping domains.

| Domain | Sources to review | Documentation or contract to update |
| --- | --- | --- |
| Configuration and CLI | `src/config.rs`, `src/cli.rs`, `compose.yaml` | [README](../README.md) environment defaults and local commands; [operations](operations.md) service, container, backup, and restore instructions |
| HTTP API and generated web types | `src/httpapi/`, `api/openapi.yaml`, `scripts/generate-api`, `web/src/lib/api/schema.d.ts`, `web/src/lib/api/client.ts` | [OpenAPI](../api/openapi.yaml) for the API contract; [README](../README.md) for API development and authentication guidance; [integration status](integrations.md) where acquisition behavior changes |
| SQLite migrations | `migrations/`, `src/store/sqlite.rs` | [Operations](operations.md) upgrade, restore, rollback, and schema compatibility guidance |
| Backup and encryption | `src/backup/`, `src/settings/encryption.rs` | [Operations](operations.md) snapshot layout, privacy, key handling, compatibility, and recovery; [integration status](integrations.md) credential-storage behavior |
| Readers and formats | `src/reader/` | [Format validation](formats.md) supported formats, limits, decoder requirements, and evidence; [operations](operations.md) container decoder dependencies |
| Providers, clients, and acquisition | `src/providers/`, `src/clients/`, `src/acquisition/` | [Integration status](integrations.md), [provider adapters](../src/providers/README.md), and [client interfaces](../src/clients/README.md); [format validation](formats.md) when transferred formats or validation change |
| Rust build and web assets | `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `build.rs`, `src/webassets.rs`, `Dockerfile`, `compose.yaml`, `scripts/check`, `tools/package.json`, `tools/bun.lock`, `web/package.json`, `web/bun.lock`, `web/vite.config.ts`, `web/src/` | [README](../README.md) build tools, commands, and embedded UI behavior; [operations](operations.md) container and runtime requirements; relevant format/integration docs for visible behavior |

Keep `api/openapi.yaml` as the API contract and generate web types through `scripts/generate-api`; never hand-edit `web/src/lib/api/schema.d.ts`. Do not create a second API inventory. `scripts/check` already regenerates types into a temporary file and compares them with the committed output.

## Change-review obligations

- For each affected domain, update the relevant docs in the PR or give a specific no-impact explanation citing the changed source and why documented behavior, defaults, commands, limits, and compatibility remain accurate. A generic “internal change” is not enough.
- Tie the assessment to the current PR-base diff and revision. Cited documentation updates must actually appear in that PR; an unchanged document or an old assessment is not evidence of an update. Reassess after source changes.
- Review the combined release range, not just the last commit. A later query-only change does not remove earlier migration or backup-format obligations.
- Manually verify local links and documented configuration against source. Keep English and Spanish locale key sets equal when changing web text. Distinguish implemented behavior, fixture-tested behavior, live evidence, and planned work; discovery or connectivity is not proof of successful transfer.

## Migration and backup compatibility

Add numbered migrations; never edit published migrations. For schema, backup-format, or encryption changes, document affected versions/formats, supported upgrade and restore paths, prerequisites, a verified backup procedure, and actionable rollback steps in [operations](operations.md). State incompatibilities and data-loss risks explicitly; explain any genuinely unaffected item rather than omitting it.

Current startup rejects a newer schema before migrating. Rollback requires retaining the previous executable/image and a verified backup, then restoring to a fresh state directory; do not run an older executable against a database migrated by a newer version. There is no backward-schema or automated-downgrade promise.

Backup preserves the database and saved encryption key, not media or environment configuration. Keep the source and published snapshot read-only during validation; migrate only disposable copies. Compatibility evidence must cover manifest acceptance, saved-key authentication, historical schema preservation, and forward migration in fresh restored state. Use disposable state and copied synthetic fixtures, never original libraries or production credentials.

## Automated checks: future work

The existing `scripts/check` runs offline after dependency installation: formatting, Clippy, Rust tests, generated API type comparison, TypeScript checking, frontend compilation, embedded-asset checks, and the release build. It does not enforce the documentation obligations above or validate all documentation links and configuration claims.

Future work under #48 is to add deterministic, offline PR/release drift checks reusing the existing contract and check script. Review the full PR-base diff with sufficient history and the combined release range; require source-current docs updates or specific no-impact explanations, and fail clearly when required tooling is missing. Local hooks would be optional supplements, not substitutes for required CI.

Focused failure fixtures should cover broken local links, stale configuration/API contracts, stale assessments or baseline resets, missing migration/backup guidance, missing tooling, staged new source files, exclusions, and combined release ranges. Any future release publication must use the same commit that passed its required gate.

These checks can enforce review obligations and verifiable facts. They cannot prove prose correct or demonstrate a live upgrade or restore. No automated documentation guard or release-readiness claim is added by this change.
