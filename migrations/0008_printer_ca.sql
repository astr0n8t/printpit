-- Optional custom CA for a printer's https:// connections: PEM text, one or more certificates.
-- When set, only these roots are trusted for that printer (see moonraker::net::Egress).
ALTER TABLE printers ADD COLUMN ca_cert TEXT;
