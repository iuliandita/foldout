# Integration status

The provider and client adapters have local protocol tests. Read-only live checks passed against Prowlarr 2.5.2.5491, SABnzbd 5.1.2, qBittorrent 5.2.3, Comic Vine, MangaUpdates, and MangaDex. No production download queues were changed during these checks.

| Integration | Implemented behavior | Remaining evidence or capability |
| --- | --- | --- |
| Prowlarr | Configured per-indexer Newznab/Torznab search, categories, capabilities, pagination, bounded payload retrieval | Live selected-payload transfer |
| SABnzbd | Authenticated queue/history, fenced submission, multiple receipts, owned-job control; isolated 5.1.3 transfer, pause/resume, and file-preserving history removal verified; comic, manga, and magazine pipeline imports and reader output verified with owned fixtures | Production transfer behavior beyond the isolated fixtures |
| qBittorrent | Authenticated API, exact hash/tag/category ownership, fenced submission, file-preserving control; isolated 5.2.3 transfer, hardlink/copy imports, and recheck without an available seed verified | v2/hybrid torrents unsupported; production transfer behavior beyond the isolated fixture |
| Comic Vine | Volume metadata search and persisted cooldowns | Broader metadata enrichment |
| MangaUpdates | Public series search with effective provider pagination | Broader identity linking |
| MangaDex | Public metadata and chapter search, explicit language/group selection, revalidated manifests, bounded complete chapter-to-CBZ transfer and journaled import; a live 12-page hosted chapter passed download, import, and reader checks | External publisher chapters require manual action |
| Internet Archive | Magazine issue discovery, exact file metadata, explicit eligibility/restriction states, and persisted search cooldowns | File transfer is not implemented; metadata eligibility does not establish access |
| GetComics | Explicit release search and selection, supported mirror resolution, bounded direct transfer, and journaled import; a live 31-page single-issue CBZ passed download, import, and reader checks | Nested multi-issue packs and unsupported mirrors require separate handling |
| Magazines | Local catalog, ISSN validation, regional editions, textual and combined issue labels, configured indexer categories; isolated real-client PDF acquisition, import, and reading verified | Broad external issue metadata is unavailable; external selected-release transfer remains unverified |

Configured credentials are encrypted in SQLite; public responses expose only configuration flags. Keep `encryption.key` with verified backups. A missing or wrong key fails startup instead of silently replacing credentials.

A new indexer acquisition requires a persisted selection decision for the caller, catalog unit, and release handle. Assess cached search or monitor results with `POST /api/v1/search/release-assessments`, record the decision with `POST /api/v1/search/release-decisions`, and pass its ID as `selection_decision_id` when creating the acquisition. Uncertain evidence requires acknowledgement of the exact assessment; conflicting releases and active rejections block selection. Use separate stable idempotency keys for the decision and acquisition.

The worker rechecks selection before committing the submission fence. Changed or expired assessments, active rejections, and legacy queued acquisitions without a decision stop for review before client submission. Previously attempted submissions retain their fence. An uncertain client response requires review and cannot be retried through the generic job endpoint. Download completion requires receipts from the configured client; catalog completion additionally requires explicit local file association and a successful import journal.

Metadata lookup, connectivity, release discovery, and successful transfer are separate results. A successful lookup must never be presented as acquisition readiness.

Direct downloads require an explicit catalog unit and destination. Interrupted transfers and changed source configuration stop for review. A completed HTTP transfer does not count as a completed acquisition until archive validation and the import journal succeed. The interface exposes supported, unresolved, manual-action, and unsupported links separately.

Contract references: [Prowlarr](https://wiki.servarr.com/en/prowlarr/quick-start-guide), [SABnzbd](https://sabnzbd.org/wiki/advanced/api), [qBittorrent](https://github.com/qbittorrent/qBittorrent/wiki), [Comic Vine](https://comicvine.gamespot.com/api/), [MangaUpdates](https://api.mangaupdates.com/), [MangaDex](https://api.mangadex.org/docs/).

Internet Archive uses anonymous access at `https://archive.org/`. Search targets the magazine collection; item identifiers represent archive entries, not authoritative publication or edition identities. Source dates, language, country, volume, and issue values remain distinct and may be missing. Inspect files before deciding availability. Restricted items, unavailable servers, unsupported formats, and incomplete file identities are shown explicitly. No sign-in, borrowing, or automatic transfer is performed.

An idle direct intent can be canceled to release its reservation while preserving files, the original failure reason, manifest evidence, and idempotency history. A new attempt requires a new explicit selection and key. Cancellation is rejected while work is active, after an import journal exists, or when the unit already has catalog coverage. Import recovery must resolve those cases first.
