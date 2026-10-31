ALTER TABLE editions ADD COLUMN language_locked INTEGER NOT NULL DEFAULT 0 CHECK (language_locked IN (0, 1));
ALTER TABLE editions ADD COLUMN region_locked INTEGER NOT NULL DEFAULT 0 CHECK (region_locked IN (0, 1));
ALTER TABLE editions ADD COLUMN publisher_locked INTEGER NOT NULL DEFAULT 0 CHECK (publisher_locked IN (0, 1));
ALTER TABLE units ADD COLUMN label_locked INTEGER NOT NULL DEFAULT 0 CHECK (label_locked IN (0, 1));
ALTER TABLE units ADD COLUMN kind_locked INTEGER NOT NULL DEFAULT 0 CHECK (kind_locked IN (0, 1));
ALTER TABLE units ADD COLUMN sort_key_locked INTEGER NOT NULL DEFAULT 0 CHECK (sort_key_locked IN (0, 1));
ALTER TABLE units ADD COLUMN date_locked INTEGER NOT NULL DEFAULT 0 CHECK (date_locked IN (0, 1));
