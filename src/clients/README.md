# Download client interfaces

`Sabnzbd` and `QBittorrent` are independent adapters used by the acquisition
pipeline. The pipeline owns durable submission fences and receipt persistence.

Dependencies: existing reqwest 0.13.5 with rustls, json, cookies, query, form,
multipart; serde, serde_json, thiserror, uuid. Tests also use existing Tokio.
No new dependency is needed.

## Root contract

- Construct `ClientConfig::new(client_id, trusted_base_url, category, limits)` only
  from administrator-owned settings. The URL is the service root, optionally with
  a reverse-proxy prefix, without an API endpoint suffix, credentials, query, or
  fragment. Use a dedicated app category. Keep the same client UUID on restart;
  changing the service behind that UUID invalidates its stored receipts.
- `Sabnzbd::new(config, api_key)` requires the full management API key.
  `QBittorrent::new(config, username, password)` stores credentials privately.
  Neither credentials, payloads, configurations, nor clients implement Debug or
  Serialize. HTTP errors discard remote text, URLs, headers, and reqwest errors.
- Both expose async `test_connection() -> Result<ConnectionInfo, ClientError>`,
  `enqueue(&mut SubmissionAttempt, AuthorizedPayload) -> Result<Vec<OwnedJob>, ClientError>`,
  `status(&OwnedJob) -> Result<JobStatus, ClientError>`,
  `queue(&[OwnedJob]) -> Result<Vec<JobStatus>, ClientError>`, and
  `pause/resume/remove(&OwnedJob) -> Result<(), ClientError>`.
- Payloads are authorized bytes: `AuthorizedPayload::nzb(bytes)` or
  `AuthorizedPayload::torrent(bytes, verified_hash)`. The acquisition layer must
  authorize the source and verify the qBittorrent identity against the torrent
  bytes, using the deployed client's hash convention (including v2/hybrid torrents).
  Constructors enforce size and hash syntax, not source authorization or bencode
  hashing. They accept no URL or magnet. Payloads are limited to 16 MiB.
- Queue accepts at most 100 stored receipts and returns their statuses, including
  completed jobs. It is not an enumeration of the user's entire queue. Any missing
  job is `NotFound`; callers needing per-item results should call `status` per job.

## Submission transaction and restart recovery

1. Under the durable intent lock, read whether the intent was ever attempted.
   Create `SubmissionAttempt::from_persisted(intent_uuid, previously_attempted)`.
2. For a fresh intent, commit an attempted/needs-review fence in the database BEFORE
   calling enqueue. Only the process holding that intent lock may submit. Persist
   client UUID, category, payload fingerprint, own ID and expected torrent hash.
3. Call enqueue once. Persist every returned OwnedJob receipt atomically. SABnzbd
   can return multiple nzo IDs. Receipts serialize only validated identifiers,
   category and client kind, with no remote names, paths, source URLs or errors.
4. After a crash, timeout, cancellation, lost response, duplicate, or `NeedsReview`,
   restore with `previously_attempted = true`. Never construct a fresh token to
   retry. Absence from a queue does not prove absence of an earlier effect.
   Reconciliation or a separately authorized new intent is required. The in-memory
   token is a replay guard, not a substitute for durable fencing.

Restore `OwnedJob::from_persisted_receipt` only from app-created database records,
never from a caller's external ID/category. It is an internal capability constructor,
not a public authorization endpoint. Every command validates client UUID, kind,
category and ID syntax, then fetches the specific remote job and checks category.
qBittorrent additionally checks the exact `libraryd-<own UUID>` tag. SAB uses the
persisted nzo ID as authoritative identity, accepting both legacy prefixed IDs and canonical non-nil UUID receipts used by SAB 5.1.3; its generated nzbname helps manual
reconciliation but is never used to adopt jobs by filename.

qBittorrent preflights the supplied hash without a category filter and rejects any
existing torrent. Serialize submissions by client/hash in root. The remote API has
no atomic add-if-absent or category-conditional mutation: external applications can
race the validation. A dedicated category and coordinated ownership are required;
this adapter cannot promise atomic isolation from other remote administrators.

All HTTP requests have a total timeout (default 20 seconds, maximum 120 seconds)
and streaming response size limit (default 2 MiB, maximum 16 MiB). Redirects,
transport retries, and environment proxies are disabled. TLS verification remains
on. No automatic retry is made after an authentication failure during a write.
Each qBittorrent operation authenticates and validates GET app/version before work,
so subsequent operations renew expired sessions without replaying mutations.

Remove always preserves files (`deleteFiles=false` / `del_files=0`). SAB history
removal is allowed only for terminal completed/failed jobs. A status-only history
delete acknowledgment additionally requires read-only confirmation that the exact
receipt is absent from queue and history; uncertain confirmation never repeats the
mutation. In-progress history
operations fail Unsupported. qBittorrent payload completion is separate from seeding,
and unknown remote states remain Unknown. Import paths need separate trusted mapping.

## Protocol evidence and validation

Official references checked September 9, 2026:

- [SABnzbd API, current 5.1 documentation](https://sabnzbd.org/wiki/configuration/5.1/api)
- [qBittorrent WebUI API 5.0+](https://github.com/qbittorrent/qBittorrent/wiki/WebUI-API-%28qBittorrent-5.0%29)
- [qBittorrent 5.2.3 authentication source](https://github.com/qbittorrent/qBittorrent/blob/release-5.2.3/src/webui/api/authcontroller.cpp)

qBittorrent supports empty successful login responses (5.2.3's verified 204) and
older `200 Ok.` responses. Add operations also accept the 5.2.3 JSON receipt only when it reports one success, no failures or pending items, and exactly the expected torrent hash. A matching hash, category, and ownership tag must still be observed afterward. Malformed or uncertain receipts preserve the attempted fence and require review. The standard cookie jar uses the actual cookie name;
After a valid add response, an absent ownership lookup is polled for at most two
seconds using GETs only, including HTTP time. Other lookup failures stop immediately;
expiry preserves NeedsReview and the attempted fence. GET version must succeed
after login. Commands use stop/start on 5.x and pause/resume
on 4.x. Unrecognized major versions fail Unsupported.

`tests/clients_http.rs` imports this module by path until root exports it. Run:
`cargo test --offline --test clients_http`. Tests use only ephemeral loopback mock
servers and synthetic credentials. They require permission to bind loopback sockets.
Separate opt-in disposable fixtures exercise real client writes without using
production configuration. See [integration status](../../docs/integrations.md)
for verified behavior and remaining evidence.
