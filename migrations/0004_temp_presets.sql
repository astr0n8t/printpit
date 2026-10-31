-- Temperature presets (admin-defined, shared by all printers). A NULL temperature means the
-- preset leaves that heater alone. Listed coolest hotend first.
CREATE TABLE temp_presets (
    id         INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    extruder   REAL,
    bed        REAL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (extruder IS NOT NULL OR bed IS NOT NULL)
);
