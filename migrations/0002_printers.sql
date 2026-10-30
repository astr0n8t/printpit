CREATE TABLE printers (
    id              INTEGER PRIMARY KEY,
    name            TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    base_url        TEXT    NOT NULL,
    -- AES-256-GCM sealed (see crypto::Cipher); NULL when the printer needs no key.
    api_key_enc     BLOB,
    webcam_override TEXT,
    enabled         INTEGER NOT NULL DEFAULT 1,
    sort_order      INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE INDEX printers_sort_order ON printers (sort_order, id);
