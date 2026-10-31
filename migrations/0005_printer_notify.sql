-- Opt-in print notifications (see notify.rs). A printer with notify = 1 keeps its Moonraker
-- connection open permanently so finished and failed prints can be reported.
ALTER TABLE printers ADD COLUMN notify INTEGER NOT NULL DEFAULT 0;
