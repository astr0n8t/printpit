-- Spoolman: printpit books each finished job's filament to the spool(s) loaded in the printer.
-- The printer never talks to Spoolman; printpit reads Moonraker's job history and calls
-- Spoolman itself.

-- Per-printer tracking settings. No row means tracking is off with one tool.
CREATE TABLE spoolman_printers (
    printer_id INTEGER PRIMARY KEY REFERENCES printers(id) ON DELETE CASCADE,
    tracking   INTEGER NOT NULL DEFAULT 0,
    -- Filament slots: extruders, or the lanes of a multi-material unit (T0, T1, …).
    tools      INTEGER NOT NULL DEFAULT 1,
    -- History cursor: only jobs that started after this (unix seconds, Moonraker's float) count.
    -- Set to "now" when tracking is turned on, then advanced past settled jobs.
    since      REAL NOT NULL DEFAULT 0,
    synced_at  INTEGER,
    -- The last sync's error, safe to show (no addresses), or NULL.
    sync_error TEXT
);

-- Which Spoolman spool is in which slot.
CREATE TABLE spoolman_slots (
    printer_id INTEGER NOT NULL REFERENCES printers(id) ON DELETE CASCADE,
    tool       INTEGER NOT NULL,
    spool_id   INTEGER NOT NULL,
    PRIMARY KEY (printer_id, tool)
);

-- One row per history job printpit has looked at, so no job is booked twice.
CREATE TABLE spoolman_bookings (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    printer_id INTEGER NOT NULL REFERENCES printers(id) ON DELETE CASCADE,
    job_id     TEXT NOT NULL,
    -- With job_id, identifies the job even if Moonraker's history is reset and ids repeat.
    job_start  REAL NOT NULL,
    filename   TEXT NOT NULL,
    job_status TEXT NOT NULL,
    job_end    REAL,
    used_mm    REAL NOT NULL,
    -- sending | booked | failed (retried) | error (Spoolman refused; not retried) | skipped |
    -- unknown (interrupted mid-send; not retried)
    status     TEXT NOT NULL,
    -- JSON: [{"tool": 0, "spool_id": 12, "length": 1520.5 | "weight": 4.6, "done": true}]
    parts      TEXT NOT NULL DEFAULT '[]',
    -- How the amounts were worked out, or why nothing was booked.
    note       TEXT NOT NULL DEFAULT '',
    -- The last Spoolman error, safe to show.
    error      TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (printer_id, job_id, job_start)
);

CREATE INDEX spoolman_bookings_recent ON spoolman_bookings (printer_id, id DESC);
CREATE INDEX spoolman_bookings_status ON spoolman_bookings (status);
