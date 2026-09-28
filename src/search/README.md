# Connected search and acquisition

`Search::new(store, settings)` uses enabled administrator-owned integration IDs.
Keep one Search instance in the HTTP context so its bounded metadata cache survives
requests. Metadata cache entries live for 15 minutes; credential-scoped request
cooldowns are persisted across restarts. Comic Vine reads are spaced 20 seconds;
other searches are spaced at least one second. Provider Retry-After extends the
durable cooldown. Search failures never mean a release or catalog unit is absent.

Release DTOs contain opaque owner-scoped handles. The database retains the source
configuration fingerprint, indexer ID, GUID digest, query and pagination. It never
retains a download descriptor, credentialed GUID/URL, NZB, or torrent bytes. Repeated
results in the same source/content-type/owner scope reuse the handle. Handles expire
for new selections after one hour. An accepted intent retains its reference.

`Pipeline::create(&settings, owner, key, request)` validates the explicit unit and
client selection and a persisted, owner-scoped selection decision, then commits a
durable intent plus job atomically. Its safe job
payload contains only the acquisition ID. Unit and optional destination reservations
prevent competing acquisitions. Destination paths live only in server-private
acquisition records. Idempotency keys are owner-scoped and conflicting bodies fail.

`POST /api/v1/search/release-assessments` assesses a cached handle against the current
catalog unit without contacting a provider. Handles created before evidence was
persisted require a new search. Unknown evidence requires acknowledgement of the
exact assessment when recording a selected decision. Conflicts and active scoped
rejections block selection. Rejection revocation appends history rather than
deleting it. New acquisition requests require `selection_decision_id`; omission
is retained only for replaying legacy intents.

The worker checks selection before fetching the payload and again inside the
submission fence transaction. Stale target, policy, evidence, or rejection state
stops unattempted work for review. An existing attempted fence is never reset.

The worker requeries the exact configured source/page and matches the GUID digest
before fetching bounded payload bytes. Missing/moved results require another search
or review; it never substitutes a similarly named result. A source or client host,
kind, category, or mapping change invalidates the saved configuration fingerprint.

Before enqueue, a SQLite transaction commits `attempted = 1`, the payload SHA-256
fingerprint, client identity/category, and optional v1 torrent hash. The acquisition
is already `needs_review` with `uncertain_submission` before any client mutation.
Only that transaction's winner can construct the fresh submission token. Every
returned receipt is stored in one transaction. A lost response, cancellation,
crash, or receipt-write failure leaves the fence intact. Generic job retry is
rejected by the job service; it cannot reset the acquisition fence.

V1 torrents are decoded with bendy 0.6.1's canonical parser. The SHA-1 input is the
original `info` dictionary byte slice from `DictDecoder::into_raw`, including its
delimiters. It is never a reserialized dictionary or the full torrent. V2/hybrid
metadata, malformed structures, unsafe standard paths and invalid piece counts
are rejected. SHA-1 is used only for the BitTorrent protocol identity; durable
payload/configuration fingerprints use SHA-256.

Root runs `Pipeline::tick(settings.clone())` in an independent worker every two
seconds. Each call processes at most one submission, receipt status, or import
journal phase. Due ordering and durable deadlines prevent a busy poll loop;
receipt polling spaces each job at least five seconds. Separate work tokens with
five-minute leases coordinate instances, and all state writes recheck the token
inside the writer transaction. Network/filesystem operations never hold that
transaction. Import steps also retain the importer's destination-root lock.

Only persisted OwnedJob receipts can be polled. Every receipt must report payload
completion before the acquisition becomes `downloaded`. That state requires an
explicit local file association. No remote path, filename guess, or inferred unit
coverage is adopted. Association chooses registered roots and relative paths; an
omitted destination reuses the immutable reservation. Import uses Copy, preserves
the source, and reuses its persisted operation UUID after restart. `completed`
requires journal phase `done` and a cataloged library file ID.

HTTP contracts are in `src/httpapi/search.rs` and `src/httpapi/acquisition.rs`.
Search, acquisition creation, file association, and safe integration/root discovery
require Manage. Acquisition list/detail require Read and are owner-scoped. Cookie
mutations use the existing origin check. Safe acquisition DTOs expose typed state
and reason codes, receipt counts, IDs and `destination_reserved`, never paths,
remote job identifiers, credentials, payloads, or provider error text.

## Validation and limits

Focused verification on September 9, 2026:

- `cargo test --offline --lib acquisition::pipeline`: 12 tests passed using
  loopback SABnzbd/qBittorrent/Prowlarr mocks and temporary databases/real CBZ files.
- `cargo test --offline --lib search::`: 3 tests passed for source/owner identity,
  restart-safe concurrent cooldowns, and canonical v1 hashing/rejection.

The fixtures cover all three content types through explicit selection, receipt
polling, file association and journal completion. They also assert the fence from
the remote mock before accepting the request, lost response across reopen,
concurrent workers/stale claims, atomic receipt rollback, target/key conflicts,
scope denial, safe discovery, and source preservation. These tests do not establish
live production interoperability; no live provider/client mutations were made.

This layer does not implement fuzzy release ranking, automatic file association,
MangaDex page packaging, GetComics resolution, remote download-path discovery, or
automatic reconciliation of uncertain submissions without receipts. Needs-review
failures are deliberately terminal here; a dedicated reviewed-resume flow for
safe polling/import recovery is not implemented. Monitoring and broader review UI
are separate integrations. Do not mark plan tasks 7-10 wholly complete from these
focused workflows alone.

Parser reference: [bendy decoding API](https://docs.rs/bendy/0.6.1/bendy/decoding/index.html).
