# Provider HTTP adapters

The default tests use ephemeral loopback servers and invented metadata. Separate
opt-in live checks validate public API interoperability. The verification history
below covers provider-layer checks; current acquisition and import evidence is
listed in [integration status](../../docs/integrations.md).

## Interface

Root owns API routes, configuration storage, caching, scheduling and persistence.
Construct one adapter per administrator-configured source. Do not deserialize
base URLs or credentials from search requests. Configurations and clients have no
Debug or Serialize implementation. A source may be an explicitly configured local
HTTP service; TLS certificate validation remains enabled for HTTPS.

`ProviderConfig::new(base_url, api_key, HttpLimits)` validates the base URL and
bounds. The base path is retained. Use the API base, including `/api/` for Comic
Vine and `/v1/` for MangaUpdates. Constructors do not guess hosts or discover URLs.
Search inputs are bounded text and pagination, never URLs to fetch.

- `ComicVine::search(query, SearchPage)` returns volume/run candidates. External
  IDs include the resource prefix, for example `4050-42`.
- `MangaUpdates::search(query, SearchPage)` posts public series search. IDs remain
  decimal strings, preserving values larger than JavaScript's exact integers.
  Novel and artbook results are excluded from manga candidates.
- `MangaDex::search(query, SearchPage)` returns manga UUIDs and localized titles.
  English is preferred, then the original language, then a deterministic fallback.
  The provider's default content-rating filter remains in effect.
- `MangaDexChapters` lists chapters for an explicit manga UUID and language, reads
  an exact chapter identity, and obtains an opaque at-home manifest. Chapter and
  volume labels remain text; group identity and page count are retained. External,
  unavailable, and empty chapters cannot produce an acquisition manifest. The
  acquisition worker separately downloads and validates every page before packaging
  a CBZ. Temporary image URLs are never serialized or stored in the database.
  Chapter listing omits `includeExternalUrl` and `includeEmptyPages`: these are
  three-state filters, where `1` selects matching chapters and `0` excludes them.
  Omitting both preserves hosted chapters alongside external and empty entries.
- `InternetArchive::search` discovers magazine archive items; `item` inspects their
  exact file metadata. These are issue/item records, not publication candidates.
  Source dates and edition fields retain their original values and uncertainty.
  Metadata-eligible files still require an access check before any future transfer.
  Restricted, unavailable, unsupported, and incomplete-identity files remain distinct.
  The adapter never fetches media, follows download redirects, signs in, or borrows.
- `MetadataPage` contains candidates, the provider's raw total, effective
  `page_size` and an optional next one-based page. Use the returned page size for
  continuation: MangaUpdates returned 25 despite a live request for 1. Counts can include filtered or
  duplicate results. Candidate dates contain only known publication years;
  missing or non-year values remain absent.
- `Prowlarr::new(config, indexer_id, protocol, CategoryMap)` configures a real
  per-indexer endpoint. `search(query, content_type, page)` or
  `search_offset(query, content_type, offset, limit)` checks `t=caps` before
  `t=search`. Unsupported categories or generic search fail explicitly.
  Categories are administrator-configured separately for each content type;
  there is no invented universal manga category. Limits above advertised caps
  fail rather than silently changing page offsets.
- `ReleasePage` preserves indexer ID, GUID, title, categories, size, protocol and
  optional RSS publication date. Enclosure/download URLs are discarded. Use
  `next_offset` for continuation. The page deduplicates GUIDs; root must deduplicate
  across pages using `(configured source, indexer_id, guid)`. A failing indexer
  returns its own error so the caller can retain other indexers' successful pages.
- `search_for_acquisition(query, content_type, offset, limit)` returns
  `AcquisitionPage` containing `AcquisitionRelease` pairs. Each pair has the safe
  `release` DTO and an optional opaque `ReleaseDownload`. The descriptor's URL is
  private; it and its containing acquisition types implement neither Debug nor
  Serialize. HTTP enclosure/link targets must match the configured source origin.
  Missing targets remain None; unsupported origins fail explicitly.
  `retrieve_payload(&descriptor)` rechecks the origin and indexer, then retrieves
  bounded bytes without redirects. It does not submit, import or parse the payload.
- `GetComics::capability()` checks only the configured landing page. It reports
  a DDL source with automated search/download both false. Explicit challenge
  markers return `ChallengeRequired`. Ordinary Cloudflare analytics or JavaScript
  detection scripts alone do not count as challenges. No scripts run, CAPTCHA
  submissions occur, or mirrors are followed.
- `getcomics::GetComicsAdapter` separately implements post search, details and
  supported link resolution. Direct acquisitions use caller-bound handles,
  source identity checks and a bounded worker. The landing-page check above is
  connectivity evidence only; it does not determine whether a selected mirror
  can complete a transfer.
  Direct archive hosts are explicitly limited to `fs3.comicfiles.ru` and
  `twlv.comicfiles.ru`. The worker validates and pins the selected host's public
  addresses; it does not follow redirects during the archive transfer.
- `magazine_identity()` creates local/manual or checksum-validated ISSN identities.
  It does not perform an ISSN registry lookup. `magazine_capability()` explicitly
  reports no external metadata search and no universal issue catalog. Magazine
  indexer results must not be interpreted as a complete issue inventory.

All HTTP calls have a whole-request timeout (default 15 seconds, maximum 60)
and streamed body cap (default 2 MiB, maximum 4 MiB). Redirects and automatic proxy
discovery are disabled. Failed requests are not automatically retried. `429`
returns `RateLimited` with parsed Retry-After seconds or IMF-fixdate; missing or
malformed headers become None. Raw HTTP/parser errors, response bodies and
credentialed URLs never become errors or public DTOs. XML DTDs, excessive nesting,
truncation and incorrect roots are rejected.

Root must cache metadata and space requests. A new client per library visit is
not a refresh policy. Persist provenance and display provider attribution. Honor
Retry-After in the job scheduler. The adapter does not synchronize Prowlarr apps,
store credentials, perform grabs, or implement download clients.

## Primary sources

- [Comic Vine API and use policy](https://comicvine.gamespot.com/api/): key-required
  reads, attribution, caching, noncommercial use and 200 requests per resource per
  hour. The web tool retrieved this policy and provider-hosted search examples;
  direct documentation fetches returned 403. The protected live adapter test
  `live_comic_vine_search` passed on September 9, 2026 (reported by root, 0.78 s).
- [MangaUpdates API](https://api.mangaupdates.com/): current embedded OpenAPI schema
  verified `POST /v1/series/search`, `page`, `perpage`, `results[].record`, IDs and
  publication year. The overview says most APIs are public despite the broad
  bearer annotation. Public series search sends no credentials. Policy requires
  attribution, reasonable spacing and caching; no numeric quota was inferred.
- [MangaDex OpenAPI](https://api.mangadex.org/docs/static/api.yaml) and
  [limitations](https://api.mangadex.org/docs/2-limitations/): manga list filters,
  pagination and publication year. Direct official-document retrieval was used
  after the web tool failed to open this schema.
- [MangaDex chapter search filters](https://gitlab.com/mangadex-pub/mangadex-api-docs/-/blob/main/04-chapter/search.md):
  optional external-URL and empty-page predicates, verified September 16, 2026.
- [Prowlarr Newznab controller](https://github.com/Prowlarr/Prowlarr/blob/develop/src/Prowlarr.Api.V1/Indexers/NewznabController.cs):
  `/{id}/api` routing and the synthetic indexer-zero behavior.
- [Torznab specification](https://torznab.github.io/spec-1.3-draft/torznab/Specification-v1.3.html)
  and [Newznab API](https://torznab.github.io/spec-1.3-draft/external/newznab/api.html):
  caps, generic search, RSS, categories, repeated attributes and pagination.
- [GetComics](https://getcomics.org/): page-based source; no stable API established.
- [Cloudflare challenge detection](https://developers.cloudflare.com/cloudflare-challenges/challenge-types/challenge-pages/detect-response/):
  `cf-mitigated: challenge` identifies challenge pages.
- [ISSN definition](https://www.issn.org/understanding-the-issn/what-is-an-issn/):
  serial identity, not a universal issue inventory.
- crates.io registry API confirmed reqwest 0.13.5 and quick-xml 0.42.0 as current
  stable versions. Root owns dependency declarations and the lockfile.

Verification: `rtk proxy cargo test --test providers_http` with loopback socket
permission: 18 local tests passed. `live_public_metadata_search` was explicitly
run with `--ignored` and passed through both Rust adapters (HTTP 200, normalized
metadata) on September 9, 2026. Live tests are opt-in so ordinary test runs never
contact providers. `live_comic_vine_search` requires a protected raw-key file via
`PROVIDER_COMICVINE_API_KEY_FILE`; root owns that credential check. No production
service or private host appears in the test fixtures. Root explicitly ran
`live_comic_vine_search` with the approved protected credential file and reported
success (0.78 s). All three metadata adapters therefore have a successful live
search check; Prowlarr search and payload retrieval remain mock-verified only.

Final checks also passed: `rtk proxy cargo clippy --lib --test providers_http --
-D warnings` and rustfmt checks for the owned Rust files. The streaming-body test
was rerun successfully after its bounded mock header reader was corrected.
