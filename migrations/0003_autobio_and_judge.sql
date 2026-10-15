-- pothos-heart v0.3: the engine's text columns join the phrase ledger; the
-- memory mirror; autobio entries with their sources; the judge's verdicts
-- and queue.

-- The ledger takes the 15 text columns: each version of a column is a row,
-- current when the latest export carried it, exactly like an array item.
ALTER TABLE pothos.observed_phrases DROP CONSTRAINT observed_phrases_dimension_check;
ALTER TABLE pothos.observed_phrases ADD CONSTRAINT observed_phrases_dimension_check
    CHECK (dimension IN (
        'interests', 'personality_traits', 'deal_breakers',
        'city', 'location', 'hometown', 'nationality', 'occupation', 'mbti_guess',
        'love_values', 'emotional_needs', 'life_rhythm', 'education', 'family',
        'relationship_history', 'social_pattern', 'future_plans', 'finance_status'));
-- id: what an autobio entry cites. recorded_at: when this service first
-- stored the row (its own clock), the autobio material watermark.
ALTER TABLE pothos.observed_phrases
    ADD COLUMN id          BIGINT GENERATED ALWAYS AS IDENTITY UNIQUE,
    ADD COLUMN recorded_at TIMESTAMPTZ NOT NULL DEFAULT now();

-- Re-walk the export once so the text columns of every user land.
UPDATE pothos.sync_cursor SET last_updated_at = NULL, last_user_id = NULL;

-- Concepts are for the three arrays only.
CREATE OR REPLACE VIEW pothos.user_concepts AS
SELECT o.user_id, o.dimension,
       COALESCE(pc.concept, o.phrase)       AS concept,
       o.last_seen_at = m.engine_updated_at AS is_current
FROM pothos.observed_phrases o
JOIN pothos.insight_mirror m USING (user_id)
LEFT JOIN pothos.phrase_decisions d USING (dimension, phrase)
LEFT JOIN pothos.phrase_concepts pc USING (dimension, phrase)
WHERE o.dimension IN ('interests', 'personality_traits', 'deal_breakers')
  AND (d.phrase IS NULL OR pc.concept IS NOT NULL);

-- Extracted memories from the engine's memories export. Insert-only there.
CREATE TABLE pothos.memory_mirror (
    id                UUID PRIMARY KEY,                    -- the engine's memory id
    user_id           UUID NOT NULL,                       -- ownership column, no FK
    content           TEXT NOT NULL,
    category          TEXT NOT NULL,
    metadata          JSONB,                               -- the extractor's hints, verbatim
    engine_created_at TIMESTAMPTZ NOT NULL,
    recorded_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_memory_mirror_user ON pothos.memory_mirror (user_id, recorded_at);

CREATE TABLE pothos.memory_cursor (
    id              SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_created_at TIMESTAMPTZ,
    last_id         UUID,
    synced_at       TIMESTAMPTZ
);
INSERT INTO pothos.memory_cursor DEFAULT VALUES;

-- One statement about a person under a fixed section. Revising retires the
-- row and adds one that points back; retired rows are the history.
CREATE TABLE pothos.autobio_entries (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id        UUID NOT NULL,                          -- ownership column, no FK
    section        TEXT NOT NULL CHECK (section IN
        ('basics', 'life', 'personality', 'love', 'intimacy', 'outlook', 'roleplay')),
    statement      TEXT NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('current', 'past', 'planned')),
    supersedes     BIGINT REFERENCES pothos.autobio_entries (id) ON DELETE SET NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    retired_at     TIMESTAMPTZ,
    retired_reason TEXT CHECK (retired_reason IN ('revised', 'retracted', 'vetoed')),
    CHECK ((retired_at IS NULL) = (retired_reason IS NULL))
);
CREATE INDEX idx_autobio_entries_user ON pothos.autobio_entries (user_id);
CREATE INDEX idx_autobio_entries_supersedes ON pothos.autobio_entries (supersedes);

-- Sources: dependent links; without either end the link means nothing.
CREATE TABLE pothos.autobio_observed_sources (
    entry_id    BIGINT NOT NULL REFERENCES pothos.autobio_entries (id) ON DELETE CASCADE,
    observed_id BIGINT NOT NULL REFERENCES pothos.observed_phrases (id) ON DELETE CASCADE,
    PRIMARY KEY (entry_id, observed_id)
);
CREATE INDEX idx_autobio_observed_sources_observed ON pothos.autobio_observed_sources (observed_id);

CREATE TABLE pothos.autobio_memory_sources (
    entry_id  BIGINT NOT NULL REFERENCES pothos.autobio_entries (id) ON DELETE CASCADE,
    memory_id UUID   NOT NULL REFERENCES pothos.memory_mirror (id) ON DELETE CASCADE,
    PRIMARY KEY (entry_id, memory_id)
);
CREATE INDEX idx_autobio_memory_sources_memory ON pothos.autobio_memory_sources (memory_id);

-- How far each user's entries have taken the material in.
CREATE TABLE pothos.autobio_state (
    user_id        UUID PRIMARY KEY,                       -- ownership column, no FK
    material_until TIMESTAMPTZ,                            -- NULL: never applied a reply
    ran_at         TIMESTAMPTZ NOT NULL,
    model          TEXT NOT NULL
);

-- A user's autobio version: the latest time any of their entries changed.
-- has_current: at least one entry stands.
CREATE VIEW pothos.autobio_versions AS
SELECT user_id,
       max(GREATEST(created_at, retired_at)) AS version,
       bool_or(retired_at IS NULL)           AS has_current
FROM pothos.autobio_entries
GROUP BY user_id;

-- The judge's grade per unordered pair, with the versions it read.
CREATE TABLE pothos.pair_verdicts (
    user_lo    UUID NOT NULL,                              -- ownership column, no FK
    user_hi    UUID NOT NULL,                              -- ownership column, no FK
    lo_version TIMESTAMPTZ NOT NULL,
    hi_version TIMESTAMPTZ NOT NULL,
    grade      TEXT NOT NULL CHECK (grade IN ('strong', 'promising', 'unclear', 'poor')),
    rationale  TEXT NOT NULL,                              -- never served
    model      TEXT NOT NULL,
    judged_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_lo, user_hi),
    CHECK (user_lo < user_hi)
);
CREATE INDEX idx_pair_verdicts_hi ON pothos.pair_verdicts (user_hi);

CREATE TABLE pothos.judge_queue (
    user_lo      UUID NOT NULL,
    user_hi      UUID NOT NULL,
    requested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_lo, user_hi),
    CHECK (user_lo < user_hi)
);
CREATE INDEX idx_judge_queue_hi ON pothos.judge_queue (user_hi);

-- Supabase lockdown, as in 0001 / 0002. Guarded so plain Postgres skips it.
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
ALTER TABLE pothos.memory_mirror            ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.memory_cursor            ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.autobio_entries          ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.autobio_observed_sources ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.autobio_memory_sources   ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.autobio_state            ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.pair_verdicts            ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.judge_queue              ENABLE ROW LEVEL SECURITY;
