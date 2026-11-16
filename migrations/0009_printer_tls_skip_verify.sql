-- Per-printer opt-out of TLS certificate verification (admin choice, shown as INSECURE).
-- When 1, any certificate the printer presents is accepted; ca_cert is ignored.
ALTER TABLE printers ADD COLUMN tls_skip_verify INTEGER NOT NULL DEFAULT 0;
