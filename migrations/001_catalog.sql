CREATE TABLE publications (
    id TEXT PRIMARY KEY,
    content_type TEXT NOT NULL CHECK (content_type IN ('comic', 'manga', 'magazine')),
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    sort_title TEXT NOT NULL CHECK (length(trim(sort_title)) > 0),
    run_label TEXT,
    title_locked INTEGER NOT NULL DEFAULT 0 CHECK (title_locked IN (0, 1)),
    known_unit_count INTEGER CHECK (known_unit_count >= 0)
);

CREATE INDEX publications_sort_title_id ON publications (sort_title, id);
CREATE INDEX publications_content_type_sort_title_id ON publications (content_type, sort_title, id);

CREATE TABLE editions (
    id TEXT PRIMARY KEY,
    publication_id TEXT NOT NULL REFERENCES publications(id) ON DELETE RESTRICT,
    language TEXT NOT NULL CHECK (length(trim(language)) > 0),
    region TEXT,
    publisher TEXT
);

CREATE INDEX editions_publication_id ON editions (publication_id, language, region, id);

CREATE TABLE units (
    id TEXT PRIMARY KEY,
    edition_id TEXT NOT NULL REFERENCES editions(id) ON DELETE RESTRICT,
    label TEXT NOT NULL CHECK (length(trim(label)) > 0),
    kind TEXT NOT NULL CHECK (kind IN ('issue', 'chapter', 'volume', 'special', 'combined')),
    sort_key TEXT,
    date TEXT,
    date_precision TEXT CHECK (date_precision IN ('year', 'month', 'day')),
    CHECK ((date IS NULL) = (date_precision IS NULL))
);

CREATE INDEX units_edition_sort ON units (edition_id, sort_key, label, id);

CREATE TABLE provider_links (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL CHECK (length(trim(provider)) > 0),
    external_id TEXT NOT NULL CHECK (length(trim(external_id)) > 0),
    publication_id TEXT REFERENCES publications(id) ON DELETE RESTRICT,
    edition_id TEXT REFERENCES editions(id) ON DELETE RESTRICT,
    unit_id TEXT REFERENCES units(id) ON DELETE RESTRICT,
    CHECK ((publication_id IS NOT NULL) + (edition_id IS NOT NULL) + (unit_id IS NOT NULL) = 1)
);

CREATE UNIQUE INDEX provider_links_publication_unique ON provider_links (provider, external_id) WHERE publication_id IS NOT NULL;
CREATE UNIQUE INDEX provider_links_edition_unique ON provider_links (provider, external_id) WHERE edition_id IS NOT NULL;
CREATE UNIQUE INDEX provider_links_unit_unique ON provider_links (provider, external_id) WHERE unit_id IS NOT NULL;

CREATE TABLE library_files (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL UNIQUE CHECK (length(trim(path)) > 0),
    format TEXT NOT NULL CHECK (format IN ('cbz', 'cbr', 'pdf')),
    signature TEXT NOT NULL CHECK (length(trim(signature)) > 0),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0)
);

CREATE TABLE file_coverage (
    library_file_id TEXT NOT NULL REFERENCES library_files(id) ON DELETE RESTRICT,
    unit_id TEXT NOT NULL REFERENCES units(id) ON DELETE RESTRICT,
    evidence TEXT NOT NULL CHECK (evidence IN ('user_confirmed', 'trusted_metadata')),
    PRIMARY KEY (library_file_id, unit_id)
);
