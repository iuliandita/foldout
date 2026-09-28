# Format validation

The application inventories and reads CBZ, CBR, and PDF files. Reviewed adoption leaves source files unchanged. Imports validate files before cataloging them and preserve source data under copy/hardlink policies.

| Format | Observed application evidence | Limits |
| --- | --- | --- |
| CBZ | Owned fixture tests and a 25-page real file: natural page order, image validation, reader progress, unchanged source | PNG, JPEG, and WebP pages; encrypted archives unsupported |
| CBR | A 187-page real file: reader, saved progress, copy import, unchanged source; RAR4/RAR5 compatibility fixtures | Requires a 7-Zip build with RAR support; encrypted archives unsupported |
| PDF | Single-page and multi-page fixtures plus a 198-page real file: manifest, bounded PNG rendering, progress, import validation | Encrypted PDFs unsupported; pages rendered at up to 2000 pixels rather than served as an unrestricted PDF |

Archive manifests allow at most 10,000 pages, 2 GiB of declared page data, and 32 MiB per page. Images are checked for bounded dimensions and pixel count. PDF manifests allow at most 10,000 pages. Decoder subprocesses have time, memory, output, and concurrency limits. Unsafe paths, linked entries, malformed manifests, and changed file identities fail explicitly.

MangaDex acquisition has separate limits: at most 1,000 pages, 32 MiB per page,
512 MiB of downloaded images, and a 15-minute transfer deadline. Each image must
decode within 16,384 pixels per dimension, 32 million pixels, and the allocation
limit. Complete chapters become CBZ files in provider page order. Temporary pages
and packaging files use private storage under the selected download root; an
incomplete chapter is never published as a successful download.

The reader caches at most 32 manifests and 64 MiB of rendered pages. Cache hits still check file identity. Reading progress uses revisions to reject conflicting writes and requires an explicit reset after file content changes. Cold decoding and warm page delivery are different measurements; representative large-file and 50,000-file results are separate acceptance checks.

Fixture provenance, hashes, dimensions, and expected order are recorded in [the manifest](../tests/fixtures/manifest.json). Test images contain only original geometric shapes. Tool success does not establish application behavior or large-file safety.

The committed CBR corpus contains small text archives for decoder compatibility only. The separate real CBR reader/import check used an authorized temporary sample that is not distributed with the project. `scripts/check-formats` runs bounded, noninteractive checks with 7-Zip and unrar. The selected primary command is 7-Zip because it is open source and its installed 26.03 build reads both Rar and Rar5. unrar 7.23 provides comparison coverage but is freeware with restrictive redistribution terms, so it is not a shipping dependency. See [the manifest](../tests/fixtures/manifest.json) for public per-fixture provenance, hashes, and licensing evidence; the required upstream license text is in `tests/fixtures/rar/LICENSE`.
