# Foldout

Foldout is a self-hosted manager for comics, magazines, and manga. The development build includes a Rust HTTP service, SQLite catalog, administrator setup, scoped API keys, durable jobs, read-only library scans, reviewed adoption, and a built-in CBZ/CBR/PDF reader. It supports configured indexer searches, download-client workflows, GetComics and MangaDex direct acquisition, and Internet Archive magazine discovery. See [integration status](docs/integrations.md) for supported behavior and remaining live-transfer checks. This is not a released replacement yet.

## Run locally

Install the Rust version declared in rust-toolchain.toml and bootstrap the pinned local build tools, then run from the repository root:

```sh
cargo fetch --locked
bun install --cwd tools --frozen-lockfile
./scripts/bun install --cwd web --frozen-lockfile
./scripts/bun run --cwd web build
cargo run --locked --features embedded-ui
```

Open http://127.0.0.1:8787. The default state directory is `./state`. Keep SQLite state on local storage.

| Environment variable | Default | Purpose |
| --- | --- | --- |
| LIBRARY_STATE_DIR | ./state | Application database directory |
| LIBRARY_LISTEN | 127.0.0.1:8787 | HTTP bind address |
| LIBRARY_ORIGIN | http://127.0.0.1:8787 | Browser origin; set to the public HTTPS origin behind a proxy |
| RUST_LOG | libraryd=info,warn | Log filtering |
| LIBRARY_WORKERS | true | Set false to stop background scans and acquisitions during recovery drills |

`GET /health/live` checks HTTP liveness. `GET /health/ready` checks database access and returns 503 when unavailable. These endpoints expose no library or credential data. The versioned API is described in [OpenAPI](api/openapi.yaml). Browser sessions use HTTP-only cookies; automation uses scoped bearer API keys. Cookie-authenticated writes require the configured origin.

Library scans read source files without moving or renaming them. Adoption requires selecting a catalog unit and accepting a preview; the source is rehashed before association. Keep original media mounted read-only while evaluating the application. Inventory shows how many catalog units are associated with the last scanned file identity; an additional association can describe another issue in a collection. Rescan after changing files.

Search the local library by title or run label, filter editions by language/region/publisher, and find units by label. Queries are literal substrings with ASCII case-insensitive matching; non-ASCII letters retain their case. Publication details allow correcting the title, sort title, run label, and known unit count. The selected edition supports language, region, and publisher corrections. Units support label, kind, sort key, and date corrections, preserving date precision. These edits keep record IDs and file associations and do not rename or move files.

Wanted lists explicit catalog units using saved associations and the last library scan. It distinguishes missing or changed files from files that have not been verified; it does not infer issues from a known count or check the live filesystem. Filter by content type, publication, title, monitoring, or availability. Publication and unit links open scoped monitors or release search with the catalog identity already selected. Monitors remain owner-scoped and review-only.

Release searches assess the selected catalog unit, with reasons for conflicts or uncertainty. Confirmation requires a saved selection decision; uncertain evidence requires explicit acknowledgement of that exact assessment, and conflicting releases cannot be selected. Rejections remain scoped to the unit, release identity, owner, and policy until explicitly revoked. Title evidence preserves issue/chapter distinctions and magazine date precision; RSS posting dates and download protocols do not establish cover dates or file formats. The worker rechecks the decision before submission. Changed or expired assessments require review. Acquisition remains manually confirmed.

## Build and check

```sh
./scripts/check
```

Checks run offline after dependency installation. The script validates formatting, Clippy, Rust tests, TypeScript, the frontend build, embedded-asset tests, and the release build. The release executable is `target/release/libraryd`; it includes its UI and needs no frontend runtime. Release builds fail without the embedded-ui feature or compiled frontend assets, including files referenced by the entrypoint.

For API development, `cargo run --locked` works without frontend assets; `/` returns an explicit 503 diagnostic. Run `./scripts/bun run --cwd web dev` for a separate development UI. The frontend uses TypeScript 7; API type generation has an isolated TypeScript 5 dependency because its compiler API is incompatible with TypeScript 7. Run `./scripts/generate-api` after editing the API contract.

SIGINT and SIGTERM initiate bounded HTTP draining and database closure. A newer database schema is rejected before migrations. `libraryd backup DESTINATION` snapshots the database and encryption key; `libraryd restore BACKUP FRESH_STATE` restores into a new directory. See [operations](docs/operations.md) for recovery and container instructions. There is no released upgrade history yet; use disposable state for development.

See [format validation](docs/formats.md) and [integration status](docs/integrations.md) for current evidence and remaining work.
