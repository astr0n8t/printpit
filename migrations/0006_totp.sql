-- TOTP second factor. The secret is sealed with crypto::Cipher (AAD "totp-secret:<user id>");
-- NULL means 2FA is off. totp_last_step is the last accepted 30 s time step, so a code can't
-- be used twice. Recovery codes are single-use and stored as SHA-256 only.
ALTER TABLE users ADD COLUMN totp_secret_enc BLOB;
ALTER TABLE users ADD COLUMN totp_last_step INTEGER;

CREATE TABLE totp_recovery (
    user_id   INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    code_hash BLOB    NOT NULL,
    PRIMARY KEY (user_id, code_hash)
);
