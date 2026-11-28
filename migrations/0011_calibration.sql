-- Calibration walkthrough (PLAN §15.13): one row per test per printer and filament profile, with
-- the slice job that printed it and the value the user read off the print.
CREATE TABLE calibration_steps (
    printer_id   INTEGER NOT NULL REFERENCES printers(id) ON DELETE CASCADE,
    filament_id  INTEGER NOT NULL REFERENCES slicer_profiles(id) ON DELETE CASCADE,
    test         TEXT NOT NULL,
    job_id       INTEGER REFERENCES slice_jobs(id) ON DELETE SET NULL,
    -- The value picked from the print (one of the test's choices), or NULL.
    result       TEXT,
    -- What that value means for the filament profile: JSON [{key, value}], computed by printpit.
    changes_json TEXT,
    -- When the changes were written to the library profile; NULL while they're only carried
    -- into the walkthrough's next tests.
    saved_at     INTEGER,
    skipped      INTEGER NOT NULL DEFAULT 0,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (printer_id, filament_id, test)
);
