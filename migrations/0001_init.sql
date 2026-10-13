-- pothos-heart v0.1 schema. Two read-only mirrors and one cursor row.
-- No foreign keys by design: user_id is an ownership column (deleting a parent
-- would destroy evidence; nulling would orphan the row), and this schema may
-- live in a database that has no table to point at anyway.

CREATE SCHEMA IF NOT EXISTS pothos;

-- Soft signal: pulled from the engine's insights export. Never written back.
CREATE TABLE pothos.insight_mirror (
    user_id            UUID PRIMARY KEY,
    interests          TEXT[] NOT NULL DEFAULT '{}',
    personality_traits TEXT[] NOT NULL DEFAULT '{}',
    deal_breakers      TEXT[] NOT NULL DEFAULT '{}',
    age_min            INT,
    age_max            INT,
    engine_updated_at  TIMESTAMPTZ NOT NULL,
    synced_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_insight_mirror_interests ON pothos.insight_mirror USING GIN (interests);
CREATE INDEX idx_insight_mirror_traits    ON pothos.insight_mirror USING GIN (personality_traits);

-- Hard attributes: pushed by the product layer. A row with active = true IS
-- the pool membership; admission, pause and exit are the product layer's.
CREATE TABLE pothos.match_profiles (
    user_id         UUID PRIMARY KEY,
    gender          TEXT NOT NULL,
    seeking_genders TEXT[] NOT NULL,
    birth_date      DATE,
    country         TEXT,
    region          TEXT,
    city            TEXT,
    active          BOOLEAN NOT NULL DEFAULT true,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Keyset position in the engine export. One row, ever.
CREATE TABLE pothos.sync_cursor (
    id              SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_updated_at TIMESTAMPTZ,
    last_user_id    UUID,
    synced_at       TIMESTAMPTZ
);
INSERT INTO pothos.sync_cursor (id) VALUES (1);

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
ALTER TABLE pothos.insight_mirror  ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.match_profiles  ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.sync_cursor     ENABLE ROW LEVEL SECURITY;
