ALTER TABLE monitors ADD COLUMN schedule_kind TEXT NOT NULL DEFAULT 'interval'
    CHECK (schedule_kind IN ('interval', 'daily', 'weekly'));
ALTER TABLE monitors ADD COLUMN schedule_time TEXT
    CHECK (schedule_time IS NULL OR (
        length(schedule_time) = 5 AND schedule_time GLOB '[0-2][0-9]:[0-5][0-9]'
        AND substr(schedule_time, 1, 2) < '24'
    ));
ALTER TABLE monitors ADD COLUMN schedule_weekday INTEGER
    CHECK (schedule_weekday IS NULL OR (
        typeof(schedule_weekday) = 'integer' AND schedule_weekday BETWEEN 1 AND 7
    ));
ALTER TABLE monitors ADD COLUMN timezone TEXT NOT NULL DEFAULT 'UTC'
    CHECK (length(timezone) BETWEEN 1 AND 255)
    CHECK (
        (schedule_kind = 'interval' AND schedule_time IS NULL AND schedule_weekday IS NULL AND timezone = 'UTC')
        OR (schedule_kind = 'daily' AND schedule_time IS NOT NULL AND schedule_weekday IS NULL)
        OR (schedule_kind = 'weekly' AND schedule_time IS NOT NULL AND schedule_weekday IS NOT NULL)
    );
