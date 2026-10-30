-- pothos-heart v0.4. The facts-ledger mirror and its cursor, the erasure
-- (tombstone) cursor, fact sources for autobio entries, and the age window's
-- move from the insights mirror to the product-pushed profile.

-- The engine's fact ledger, as exported. id is the engine's fact id;
-- recurrence stores the engine's derived tier. recorded_at is this service's
-- clock and the autobio material watermark: it moves only when the engine row
-- actually changed (its last_seen_at advanced), so a re-seen fact is new
-- material while an overlap re-fetch is not.
CREATE TABLE pothos.fact_mirror (
    id              UUID PRIMARY KEY,
    user_id         UUID NOT NULL,                          -- ownership column, no FK
    content         TEXT NOT NULL,
    category        TEXT NOT NULL,
    domain          TEXT NOT NULL,
    evidence_type   TEXT NOT NULL,
    temporality     TEXT NOT NULL,
    persistence     TEXT NOT NULL,
    confidence      TEXT NOT NULL,
    occurrences     INT NOT NULL,
    session_count   INT NOT NULL,
    recurrence      TEXT NOT NULL,
    first_seen_at   TIMESTAMPTZ NOT NULL,
    last_seen_at    TIMESTAMPTZ NOT NULL,
    recorded_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_fact_mirror_user ON pothos.fact_mirror (user_id, recorded_at);

-- Keyset position in the facts export. One row, ever.
CREATE TABLE pothos.fact_cursor (
    id              SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_seen_at    TIMESTAMPTZ,
    last_id         UUID,
    synced_at       TIMESTAMPTZ
);
INSERT INTO pothos.fact_cursor DEFAULT VALUES;

-- Keyset position in the erasure (tombstone) stream. One row, ever.
CREATE TABLE pothos.erasure_cursor (
    id              SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_erased_at  TIMESTAMPTZ,
    last_user_id    UUID,
    synced_at       TIMESTAMPTZ
);
INSERT INTO pothos.erasure_cursor DEFAULT VALUES;

-- Entry <-> fact material. Both dependent links, both CASCADE, like the two
-- source tables in 0003.
CREATE TABLE pothos.autobio_fact_sources (
    entry_id  BIGINT NOT NULL REFERENCES pothos.autobio_entries (id) ON DELETE CASCADE,
    fact_id   UUID   NOT NULL REFERENCES pothos.fact_mirror (id) ON DELETE CASCADE,
    PRIMARY KEY (entry_id, fact_id)
);
CREATE INDEX idx_autobio_fact_sources_fact ON pothos.autobio_fact_sources (fact_id);

-- The age window moves home: the engine no longer exports an LLM-guessed one,
-- the product layer pushes the user's own.
ALTER TABLE pothos.insight_mirror DROP COLUMN age_min, DROP COLUMN age_max;
ALTER TABLE pothos.match_profiles ADD COLUMN age_min INT, ADD COLUMN age_max INT;

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
ALTER TABLE pothos.fact_mirror          ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.fact_cursor          ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.erasure_cursor       ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.autobio_fact_sources ENABLE ROW LEVEL SECURITY;
