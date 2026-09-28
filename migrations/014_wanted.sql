CREATE INDEX file_coverage_unit_file ON file_coverage(unit_id, library_file_id);
CREATE INDEX units_edition_effective_sort ON units(edition_id, COALESCE(sort_key, label), id);
