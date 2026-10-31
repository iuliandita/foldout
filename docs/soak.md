# Disposable soak protocol

This opt-in workload prepares evidence for issue #100. A fixture pass establishes
only the observed owned-fixture window. **Real-household acceptance stays
`unmeasured` until the owner separately authorizes and records a household
observation.** Production downloader permissions remain read-only. This command
never uses a production downloader, media library, NAS original, or saved state.
Automatic monitor selection (#96) is outside this protocol: monitors use
`review_only`, and every acquisition has an explicit selection.

## Preconditions and commands

Use Linux, Python 3.11 or newer, the pinned Rust toolchain, local storage, and the
reader's `7z` and `prlimit` dependencies. Start from a known source revision. Fetch
locked dependencies once before offline builds. Keep evidence private: reports
include disposable paths, object IDs, process IDs, and service diagnostics. The
runner strips inherited service configuration and proxy settings; credentials
are fresh fixture values. No existing credentials or browser sessions are used.

```sh
umask 077
export CARGO_TARGET_DIR="$PWD/target" CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=1
cargo fetch --locked
nice -n 8 cargo build --locked --offline --bin libraryd --features soak-fixtures
./scripts/soak --state-dir docs/local/soak-smoke --duration-seconds 180
```

The default is a 180-second smoke window, not multi-day evidence. `--state-dir`
must not exist, including as an empty directory or symlink; its parent must
exist on trusted local storage. The service creates a new private `state/`
beneath it, listens on an unused literal loopback port, and enables workers.
`--binary` can select the debug executable. `--listen-url` accepts only a literal
loopback HTTP origin with a port, without credentials, query, fragment or path.
The runner refuses other configured URLs, occupied listen ports, binaries without
the `soak-fixtures` debug version marker, and redirects. Fixture servers
bind only to `127.0.0.1`; they record requests and never forward traffic.

Direct archive policy normally rejects loopback. The explicit `soak-fixtures`
Cargo feature reuses the test transport override: `LIBRARY_SOAK_ADDRESS` must be
a literal loopback socket, and the archive connection is rewritten there before
DNS or connect. The fixture's allowed-host archive link is an inert adapter
identity, not a contacted endpoint. No host policy is relaxed in ordinary builds.
The debug version string includes `[soak-fixtures debug]`; the runner checks it
before starting any workload. Release builds with this feature are rejected at
compile time. Never deploy this
debug executable. The smoke exercises GetComics-style archives; MangaDex pages,
real indexers and real downloaders remain outside its evidence.

For a bounded multi-day fixture profile, choose a fresh path:

```sh
./scripts/soak --state-dir docs/local/soak-72h \
  --duration-seconds 259200 --interval-seconds 900
```

Duration accepts 60 through 604800 seconds (seven days). The entire setup and
workload must fit in that window. The default interval is a 900-second idle gap
between completed cycles, with no backlog or concurrent cycles. During the gap,
readiness is checked every second. After the first cycle, no new cycle starts
with fewer than 90 seconds remaining. A cycle that cannot finish before the deadline
fails; a shorter duration is never silently extended. There is no long-running CI
job. Running the smoke locally or in a CI sandbox requires the explicit command.

## Exact scheduled workload

Each cycle uses fresh catalog units and destination filenames, in this order:

1. Select an owned direct archive through search/details. Cut its first body in
   half after sending the full Content-Length, byte-range support and strong
   ETag. Observe `needs_review`, gracefully restart the owned service, fetch its
   persisted review snapshot, and send its exact revision/fingerprint to `resume`.
   The fixture honors Range/If-Range with an exact 206. Require `completed` and
   byte-identical imported output.
2. Cut another selected direct archive. Resume with a changed ETag and require
   `needs_review` with `transfer_restart_required`. Fetch a fresh snapshot and
   explicitly restart. Replay that exact restart and require the same replacement
   ID and a canceled original. Require the replacement to complete with exact
   fixture bytes. These are reviewed actions, never automatic transfer retries.
3. Run one review-only monitor against an owned fake Newznab indexer. Require
   `needs_review` candidates with no download-client submission. Assess and
   explicitly select one release with its exact assessment acknowledgement.
4. Create an indexer acquisition while holding the fake payload response. Correct
   its edition region through the catalog endpoint, then release the body. Require
   `selection_review_required` before submission. Assess the cached release again
   and use the terminal review revision/fingerprint to reassess. Require a canceled
   original and no client writes. Create a new intent using the returned decision.
   This tests changed-target reassessment, not natural one-hour expiration.
5. The recording fake SABnzbd accepts exactly one submission and returns an owned
   completed receipt. Explicitly associate a missing local filename, observe
   `import_review_required`, and correct mapping through its fenced review action.
6. Temporarily change only the disposable destination's `.library-imports`
   permissions to 0755. Import planning succeeds but staging refuses those unsafe
   permissions. Observe `import_review_required`, restore 0700, and resume the
   existing journal using a fresh review. Read-only journal inspection requires
   the same ID, Copy policy, and progression from planned to done. Require `completed`, exact destination
   bytes and no additional client submission. Source policy remains Copy.
7. Disable that cycle's monitor. Record completion, then wait for the next cycle
   or the end of the observation window.

All API calls use existing endpoints in [the contract](../api/openapi.yaml).
Polling records observed state changes and reasons; very short intermediate
states may be missed. The runner does not claim that polling is a complete trace.
Injected faults and reviewed actions are part of unattended scripted operation,
not evidence that a human household can recover without review.

## Originals, bounds, and stopping

The only source is the repository-owned `natural-order.cbz` fixture, read as
bytes into independent files, never hardlinked or reflinked. `originals/owned.cbz`
and `source/owned.cbz` are mode 0400 in mode 0500 directories. Downloads and
imports have separate writable roots. SHA-256 manifests before and after cover
the repository fixture, read-only original copy, and import source. No NAS path
is accepted. Read-only permissions are protection against accidental writes,
not a security sandbox against the account that owns the files.

Abort on an unexpected HTTP response, malformed evidence, missing transition,
phase timeout (40 seconds; startup 30 seconds), changed original hash, duplicate
submission, wrong imported bytes, unsafe restart replay, service exit, footprint
over 256 MiB, or service RSS over 1 GiB. API requests have at most 20 seconds and
are also bounded by the remaining window. Disk/RSS are polled between operations;
transient peaks and decoder descendants are not continuously accounted for.
The fake payload gate releases or times out after 20 seconds. No external network
acceptance or resource-performance claim follows from these bounds.

Use Ctrl-C or SIGTERM to abort. The runner terminates only its recorded child,
allows 35 seconds for shutdown, and closes its owned fixture servers. A forced
service kill is a failure. Evidence and disposable state are retained; nothing
is automatically deleted. SIGKILL or host failure can leave an owned service and
partial report: inspect its PID and `service.log`, stop that exact process, and
keep the incomplete run invalid/unmeasured. Do not reuse its state directory.
Check retained evidence before removing only that disposable run directory.

## Evidence and classifications

The run directory contains atomic `report.json` checkpoints, `summary.txt`, and
`service.log`. [The report template](soak-report-template.json) intentionally
contains no passing measurements. Reports record source revision, dirty-tree
status, executable SHA-256/profile, exact requested/elapsed window, cycle count,
UTC endpoints, monotonic transition offsets, reasons, injected faults, fixture
request method/path/Range/If-Range, client submissions, hash manifests, failures,
and process/server cleanup. They never record session cookies or setup tokens.

```sh
./scripts/soak --classify docs/local/soak-smoke/report.json
python3 -I -B -m unittest discover -s tests -p soak_report_test.py
```

- `pass`: a complete bounded fixture cycle, full requested window, all required
  checks and invariants, unchanged originals, and successful process cleanup.
- `fail`: recorded unexpected failures, violated invariants, changed originals,
  or failed cleanup. The runner exits nonzero. Expected injected interruptions
  are transitions, not unexpected failures.
- `unmeasured`: well-formed evidence without complete workload/window evidence.
  A sub-two-day run always leaves multi-day observation unmeasured.
- `invalid`: absent, malformed, or structurally incomplete evidence. The template
  is invalid until filled from observation. Classification exits nonzero for
  invalid/fail; an actual run also exits nonzero for unmeasured.

Synthetic multi-day observation needs at least 172800 requested and observed
seconds and a passing complete workload. Even then, real-household acceptance is
always `unmeasured`, regardless of caller-supplied household fields. The owner
owns separate authorization and observation of household behavior, real transfer
services, natural expiration, longer outages, host restarts and real libraries.
A fixture report cannot establish those results.
