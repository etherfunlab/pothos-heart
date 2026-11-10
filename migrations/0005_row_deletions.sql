-- pothos-heart v0.5. The engine's row-deletion stream: its cursor, the
-- tombstones already consumed, and a retire reason for entries whose last
-- source was deleted.

-- Keyset position in the row-deletion stream. One row, ever.
CREATE TABLE pothos.row_deletion_cursor (
    id              SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_deleted_at TIMESTAMPTZ,
    last_id         UUID,
    synced_at       TIMESTAMPTZ
);
INSERT INTO pothos.row_deletion_cursor DEFAULT VALUES;

-- Tombstones consumed so far. The id is kept so a changes page that was read
-- before the delete and arrives after it cannot bring the row back. No
-- user_id: the engine's tombstone carries none, and a user's erasure leaves
-- this table alone.
CREATE TABLE pothos.row_tombstones (
    kind       TEXT NOT NULL,                       -- 'memory' | 'fact'
    id         UUID NOT NULL,                       -- the row's id in its changes stream
    deleted_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (kind, id)
);

-- An entry retired because its last source was deleted, apart from one the
-- model retracted.
ALTER TABLE pothos.autobio_entries
    DROP CONSTRAINT autobio_entries_retired_reason_check,
    ADD  CONSTRAINT autobio_entries_retired_reason_check
         CHECK (retired_reason IN ('revised', 'retracted', 'vetoed', 'source_deleted'));

-- Supabase lockdown, guarded so plain Postgres (incl. the test DB) skips it.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON ALL TABLES IN SCHEMA pothos FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON ALL TABLES IN SCHEMA pothos FROM authenticated;
    END IF;
END
$$;
ALTER TABLE pothos.row_deletion_cursor ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.row_tombstones      ENABLE ROW LEVEL SECURITY;
