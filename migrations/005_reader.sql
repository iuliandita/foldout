CREATE TABLE reading_progress (
    user_id TEXT NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
    file_id TEXT NOT NULL REFERENCES library_files(id) ON DELETE CASCADE,
    signature TEXT NOT NULL,
    page INTEGER NOT NULL CHECK (page >= 0 AND page < 10000),
    direction TEXT NOT NULL CHECK (direction IN ('ltr', 'rtl', 'vertical')),
    revision INTEGER NOT NULL CHECK (revision > 0),
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, file_id)
);
