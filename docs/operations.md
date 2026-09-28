# Operations

## Container

Build locally with `docker compose build`, then start with `docker compose up -d`.
The local image is `libraryd:0.1.0`, matching the Cargo package version. No image is
published by these commands. Open http://localhost:8787. The host port binds only
to loopback; put an HTTPS reverse proxy in front for remote access and set
`LIBRARY_ORIGIN` to the exact public browser origin before starting Compose.

The image runs as UID/GID 1000, with a read-only root filesystem under Compose.
Its entrypoint sets umask 077 and replaces itself with the service process, so
new database and journal files remain private and shutdown signals reach the service.
The named `state` volume holds `/state`; `/tmp` is a bounded temporary filesystem.
The image creates `/state` with mode 700 and ownership 1000:1000. Docker initializes
a new named volume from this directory. For a bind mount, prepare the host directory
with the same owner and permissions before starting. Existing volumes are not
automatically repaired. Keep SQLite on local storage, not NFS or SMB. Mount media
roots separately, read-only for scans; explicitly grant write access only to roots
intended for imports. Preserve the same media paths when recovering a catalog.

The build pins Rust 1.98.1, Bun 1.4.2, and Alpine 3.24.2 with registry manifest
digests. Build tools install separately under `tools/`, including TypeScript 5 for
the API generator. Frontend TypeScript 7 stays under `web/`. The build checks the
generated API types, type-checks and compiles the frontend, and embeds its assets
in the release executable. The runtime contains neither Bun nor Rust.

Alpine packages supply
[poppler-utils](https://pkgs.alpinelinux.org/package/v3.24/main/x86_64/poppler-utils)
and [util-linux-misc](https://pkgs.alpinelinux.org/package/v3.24/main/x86/util-linux-misc)
for `pdfinfo`, `pdftoppm`, and `prlimit`. Alpine's `7zip` package omits RAR decoding,
so the image uses the static `7zzs` executable from
[upstream 7-Zip 26.03](https://github.com/ip7z/7zip/releases/tag/26.03), installed
as `/usr/bin/7z`. Its release archives are SHA-256 pinned separately for amd64
and arm64; other architectures fail explicitly. The upstream license and readme
are retained under `/usr/share/licenses/7zip/`. Runtime
package updates come from the pinned Alpine release's repositories at build time;
their versions are not locked. Rebuild regularly for security updates.

`GET /health/ready` is the container health check. Inspect startup with
`docker compose logs --tail=100 library`. Compose allows 30 seconds for graceful
shutdown and keeps bounded logs. Do not use `docker compose down --volumes` on
state you need to retain.

A development image for amd64 and arm64 is published from every `develop` push as
`ghcr.io/iuliandita/foldout:nightly` (and `:nightly-<short sha>`). It is a moving,
unreleased channel: expect breaking changes and keep a verified backup. The image
is signed with cosign keyless signing and carries SBOM and provenance attestations:

```sh
cosign verify ghcr.io/iuliandita/foldout:nightly \
  --certificate-identity-regexp '^https://github.com/iuliandita/foldout/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

There are no tagged releases yet.

## Initial administrator

Fresh instances require a setup token before an administrator
can be created. Read `setup-token` from the private state directory and enter it in
the setup screen. In Compose, use `docker compose exec library cat /state/setup-token`.
The token is never logged and is removed after successful setup. Configured startup
also removes a leftover token from an interrupted setup cleanup.
API clients send it as `X-Setup-Token`; setup status reports `requires_token`.
This also protects loopback instances exposed through a reverse proxy. Keep the state directory
service-owned with mode 0700; startup rejects insecure directories before writing
or migrating the database.

## Backup commands

Use an existing state directory as the backup source and absent destination paths:

```sh
LIBRARY_STATE_DIR=/srv/library/state libraryd backup /srv/backups/library-2026-09-09
libraryd restore /srv/backups/library-2026-09-09 /srv/library/restored-state
```

`backup DESTINATION` takes its source from `LIBRARY_STATE_DIR` and requires
existing state. `restore BACKUP FRESH_STATE` takes both paths explicitly. A command
failure must be treated as an unusable backup or restore until inspected. Run
`libraryd --help` for usage and `libraryd --version` for the executable version.
`libraryd serve`, or no subcommand, starts the service. The restored service can
be started with `LIBRARY_STATE_DIR=/srv/library/restored-state libraryd serve`;
choose a separate `LIBRARY_LISTEN` and matching `LIBRARY_ORIGIN` for a drill.

The exported service functions are `backup_existing(state_dir, destination)`,
`backup(&SqliteStore, state_dir, destination)`, and
`restore(backup_dir, fresh_state_dir)`, returning `Result<(), BackupError>`.
The store-taking wrapper additionally verifies that the source belongs to that store.

The caller must use a trusted local destination parent directory, restrict access
to the account running the service, and supply a destination that does not exist.
The parent must already exist. Both functions reject existing destinations,
including empty directories and symbolic links. The supplied state directory must
match the store's database. POSIX filesystem permissions are required.

Backup uses a parameter-bound `VACUUM INTO ?` on an independent read-only
connection to the existing database. It does not create or migrate source state. It runs outside a write transaction and needs no SQLite CLI or unsafe native
handle access. [SQLite documents VACUUM INTO](https://www.sqlite.org/lang_vacuum.html)
as a consistent snapshot of a live database, including committed WAL changes.
Transactions committed after its snapshot are not included. It can require
substantial I/O, CPU, and free disk space; schedule accordingly. Do not copy a live
database file by itself or discard its WAL.

The backup directory is created exclusively with mode 700. Its `.pending/`
directory holds the database, master key when present, and versioned manifest.
Files are created with mode 600 before any contents are written. Validation opens
a separate private copy through `SqliteStore`, checks SQLite integrity and foreign
keys, and authenticates all encrypted settings with the saved key. Only this
disposable validation copy is migrated; the source and published snapshot retain
their original schema. Successful validation removes its temporary copy. The database and key
SHA-256 checksums are recorded in the manifest. Files and directories are synced
before `.pending/` is atomically renamed to `snapshot/`, followed by directory
syncs. Restore accepts only a published `snapshot/`.

The completed layout is:

```text
backup-directory/             700
  snapshot/                  700
    library.sqlite3          600
    encryption.key           600 (when the source has a key)
    manifest.json            600
```

The SQLite snapshot retains encrypted settings. The accompanying master key can
decrypt those settings: **keep the entire backup private** and use encrypted
storage or an encrypted backup transport for off-host copies. Database checksums
detect accidental corruption; they do not authenticate a backup against an
attacker who can replace its manifest. Preserve directory and file permissions
when copying backups. Treat backups as trusted input protected from tampering.

A missing master key is permitted only when there are no encrypted settings rows.
Backup and restore never generate a replacement for a missing saved key. A key
with incorrect size, insecure permissions, or a symlink is rejected. Restore
checks key identity against the manifest and authenticates settings again, so a
different key cannot silently produce a usable restore. Do not rotate or replace
the source key while a backup runs.

These functions back up the database and key, not media files, environment
configuration, reverse proxy settings, or files in the middle of an import. Back
up media separately and stop acquisition/import workers when a coordinated
database-and-media recovery point is required. Copy only complete `snapshot/`
trees. Keep at least one verified recovery point outside the state disk.

## Restore drill and upgrade

1. Choose a trusted backup and an absent local state directory. Keep the source
   service and original state untouched during a restore drill.
2. Run `libraryd restore BACKUP FRESH_STATE` and require a successful exit.
   It copies into private files, verifies checksums and key identity, then opens
   through `SqliteStore` to enforce the startup migration checks, including the
   maximum supported migration version. It also checks database integrity,
   foreign keys, and credential authentication. An older supported database may
   be migrated in the restored copy; the backup remains unchanged.
3. Start an isolated service against the restored directory with a separate listen
   port and appropriate origin. Set `LIBRARY_WORKERS=false` during drills to keep
   scheduled scans and acquisitions stopped. This does not disable interactive
   provider checks or API writes. Check readiness, sign-in, catalog rows, encrypted integration
   settings, and a representative media read. Confirm media paths still resolve.
4. For actual recovery, stop the old service before switching its state mount to
   the verified restored directory. Preserve the original state as a rollback copy.
   Never restore over the active database.

Errors or interruptions can leave a private destination directory behind. A failed
backup lacks `snapshot/` and cannot be restored. A failed restore may contain a
partial database or key: do not start it. Inspect and quarantine these paths, then
retry with a new absent directory. No automatic cleanup deletes existing user
directories. A final sync failure is reported even if publication already occurred;
verify or recreate the backup before relying on it.

Before upgrading, retain a verified backup and the previous executable/image.
Restore to a fresh directory for rollback; do not point an older executable at a
database migrated by a newer version. There is no promise of backward schema
compatibility or automated downgrade.

## Verification

Run `cargo test --locked --offline --test backup_restore`. It imports the exported
backup API and covers WAL snapshots, encrypted settings recovery, private
permissions, missing/wrong keys, corruption, existing destinations, source/store
mismatch, incomplete backups, symlinks, newer-schema rejection, historical schema
preservation, and forward migration only in the fresh restored state. Run
`docker compose config --quiet` to validate Compose and `docker compose build`
to exercise the full asset and release build on the host architecture.

The repository-wide acceptance check remains `./scripts/check`.
