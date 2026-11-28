-- Calibration runs (PLAN §15.13): a calibration is started explicitly on a printer with a process
-- and a filament and a chosen list of tests, and keeps its own progress. Replaces the
-- per-printer-and-filament progress of 0011.
CREATE TABLE calibration_runs (
    id          INTEGER PRIMARY KEY,
    printer_id  INTEGER NOT NULL REFERENCES printers(id) ON DELETE CASCADE,
    -- NULL when the process was deleted: the printer's default process is used instead.
    process_id  INTEGER REFERENCES slicer_profiles(id) ON DELETE SET NULL,
    filament_id INTEGER NOT NULL REFERENCES slicer_profiles(id) ON DELETE CASCADE,
    user_id     INTEGER REFERENCES users(id) ON DELETE SET NULL,
    -- The tests chosen, as comma-separated slugs.
    tests       TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    -- Set when every chosen test has a result or was skipped.
    finished_at INTEGER
);
CREATE INDEX calibration_runs_by_update ON calibration_runs (updated_at);

CREATE TABLE calibration_run_steps (
    run_id       INTEGER NOT NULL REFERENCES calibration_runs(id) ON DELETE CASCADE,
    test         TEXT NOT NULL,
    job_id       INTEGER REFERENCES slice_jobs(id) ON DELETE SET NULL,
    -- The value picked from the print (one of the test's choices), or NULL.
    result       TEXT,
    -- What that value means for the profiles: JSON [{kind, key, value}], computed by printpit.
    changes_json TEXT,
    -- When the changes were written to the library profiles; NULL while they're only carried
    -- into the run's next tests.
    saved_at     INTEGER,
    skipped      INTEGER NOT NULL DEFAULT 0,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (run_id, test)
);

-- The first walkthrough's progress (one per printer and filament) becomes one run each, with the
-- six core tests and any optional ones it had started.
INSERT INTO calibration_runs (printer_id, process_id, filament_id, tests, created_at, updated_at)
SELECT s.printer_id,
       (SELECT default_process_id FROM printer_slicer ps WHERE ps.printer_id = s.printer_id),
       s.filament_id,
       'temperature,flow_coarse,flow_fine,pressure_advance,retraction,volumetric_speed' ||
           COALESCE(',' || (SELECT group_concat(x.test) FROM calibration_steps x
                            WHERE x.printer_id = s.printer_id AND x.filament_id = s.filament_id
                              AND x.test NOT IN ('temperature', 'flow_coarse', 'flow_fine',
                                                 'pressure_advance', 'retraction', 'volumetric_speed')),
                    ''),
       MIN(s.updated_at),
       MAX(s.updated_at)
FROM calibration_steps s
GROUP BY s.printer_id, s.filament_id;

INSERT INTO calibration_run_steps (run_id, test, job_id, result, changes_json, saved_at, skipped, updated_at)
SELECT r.id, s.test, s.job_id, s.result, s.changes_json, s.saved_at, s.skipped, s.updated_at
FROM calibration_steps s
JOIN calibration_runs r ON r.printer_id = s.printer_id AND r.filament_id = s.filament_id;

DROP TABLE calibration_steps;
