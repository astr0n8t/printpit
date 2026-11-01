-- Slicing (PLAN §15). Profiles are stored flattened (the worker resolves `inherits`), so a
-- profile is self-contained. Profiles hold start/end G-code: only admins edit them.
CREATE TABLE slicer_profiles (
    id            INTEGER PRIMARY KEY,
    kind          TEXT    NOT NULL CHECK (kind IN ('machine', 'process', 'filament')),
    name          TEXT    NOT NULL COLLATE NOCASE,
    -- 'system': imported from the worker's bundled Orca library; 'user': an imported preset.
    source        TEXT    NOT NULL CHECK (source IN ('system', 'user')),
    -- The Orca vendor a system profile came from (NULL for user presets).
    vendor        TEXT,
    settings_json TEXT    NOT NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    UNIQUE (kind, name)
);

-- Which machine profile a printer slices with, and the defaults its form starts on. A profile
-- in use as a machine can't be deleted (RESTRICT); a deleted default just clears.
CREATE TABLE printer_slicer (
    printer_id          INTEGER PRIMARY KEY REFERENCES printers (id) ON DELETE CASCADE,
    machine_profile_id  INTEGER NOT NULL REFERENCES slicer_profiles (id) ON DELETE RESTRICT,
    default_process_id  INTEGER REFERENCES slicer_profiles (id) ON DELETE SET NULL,
    default_filament_id INTEGER REFERENCES slicer_profiles (id) ON DELETE SET NULL,
    updated_at          INTEGER NOT NULL
);

-- One upload, sliced for one printer. Files live in <data>/slicer/<id>/. A job keeps its row
-- (and stats) when its user, printer or profiles are deleted later.
CREATE TABLE slice_jobs (
    id                 INTEGER PRIMARY KEY,
    user_id            INTEGER REFERENCES users (id) ON DELETE SET NULL,
    printer_id         INTEGER REFERENCES printers (id) ON DELETE SET NULL,
    -- Cleaned file name as uploaded, e.g. `benchy.stl`; its extension says what the model is.
    model_name         TEXT    NOT NULL,
    model_size         INTEGER NOT NULL,
    status             TEXT    NOT NULL
        CHECK (status IN ('queued', 'slicing', 'uploading', 'done', 'failed', 'cancelled')),
    machine_profile_id INTEGER REFERENCES slicer_profiles (id) ON DELETE SET NULL,
    process_id         INTEGER REFERENCES slicer_profiles (id) ON DELETE SET NULL,
    filament_id        INTEGER REFERENCES slicer_profiles (id) ON DELETE SET NULL,
    -- Overrides and switches chosen on the form (slicer::options), as JSON.
    options_json       TEXT    NOT NULL DEFAULT '{}',
    -- Start the print once uploaded (re-checked against the printer's state first).
    start_print        INTEGER NOT NULL DEFAULT 0,
    -- Where the G-code went on the printer, relative to the gcodes root.
    output_name        TEXT,
    est_seconds        INTEGER,
    filament_mm        REAL,
    filament_g         REAL,
    layers             INTEGER,
    gcode_size         INTEGER,
    error              TEXT,
    log_tail           TEXT,
    created_at         INTEGER NOT NULL,
    started_at         INTEGER,
    finished_at        INTEGER
);
CREATE INDEX slice_jobs_status ON slice_jobs (status, id);
CREATE INDEX slice_jobs_printer ON slice_jobs (printer_id, id);
CREATE INDEX slice_jobs_user ON slice_jobs (user_id, id);
