-- Users, sessions, preferences and the audit log. Timestamps are unix seconds (UTC).

CREATE TABLE users (
    id            INTEGER PRIMARY KEY,
    username      TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT    NOT NULL,
    role          TEXT    NOT NULL CHECK (role IN ('admin', 'operator', 'viewer')),
    disabled      INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

-- The session ID itself never touches the database; only its SHA-256.
CREATE TABLE sessions (
    id_hash      BLOB    PRIMARY KEY,
    user_id      INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    csrf_token   TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    user_agent   TEXT,
    ip           TEXT
);
CREATE INDEX sessions_user_id ON sessions (user_id);

CREATE TABLE user_prefs (
    user_id             INTEGER PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    selected_printer_id INTEGER,
    theme               TEXT,
    auto_refresh        INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE audit_log (
    id         INTEGER PRIMARY KEY,
    user_id    INTEGER REFERENCES users (id) ON DELETE SET NULL,
    printer_id INTEGER,
    action     TEXT    NOT NULL,
    detail     TEXT,
    ip         TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX audit_log_created_at ON audit_log (created_at);
