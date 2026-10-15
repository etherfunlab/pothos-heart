-- pothos-heart v0.2: the phrase ledger and the shared concept vocabulary.
-- The mirror keeps the engine's scalars; every array item the engine has ever
-- exported lives in observed_phrases, which only grows. Concepts are decided
-- once per (dimension, phrase), for every user who wrote that phrase.

CREATE TABLE pothos.observed_phrases (
    user_id       UUID NOT NULL,
    dimension     TEXT NOT NULL
        CHECK (dimension IN ('interests', 'personality_traits', 'deal_breakers')),
    phrase        TEXT NOT NULL,
    first_seen_at TIMESTAMPTZ NOT NULL,
    last_seen_at  TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (user_id, dimension, phrase)
);
CREATE INDEX idx_observed_phrases_phrase ON pothos.observed_phrases (dimension, phrase);

INSERT INTO pothos.observed_phrases (user_id, dimension, phrase, first_seen_at, last_seen_at)
SELECT user_id, 'interests', p, engine_updated_at, engine_updated_at
FROM pothos.insight_mirror, unnest(interests) AS p
UNION ALL
SELECT user_id, 'personality_traits', p, engine_updated_at, engine_updated_at
FROM pothos.insight_mirror, unnest(personality_traits) AS p
UNION ALL
SELECT user_id, 'deal_breakers', p, engine_updated_at, engine_updated_at
FROM pothos.insight_mirror, unnest(deal_breakers) AS p
ON CONFLICT DO NOTHING;

-- The GIN indexes on interests / personality_traits go with their columns.
ALTER TABLE pothos.insight_mirror
    DROP COLUMN interests,
    DROP COLUMN personality_traits,
    DROP COLUMN deal_breakers;

-- One vocabulary for all three dimensions: deal-breakers are compared with
-- the other side's interests and traits.
CREATE TABLE pothos.concepts (
    label      TEXT PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A row with no phrase_concepts rows means "decided: no concept".
CREATE TABLE pothos.phrase_decisions (
    dimension  TEXT NOT NULL
        CHECK (dimension IN ('interests', 'personality_traits', 'deal_breakers')),
    phrase     TEXT NOT NULL,
    model      TEXT NOT NULL,
    decided_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (dimension, phrase)
);

CREATE TABLE pothos.phrase_concepts (
    dimension TEXT NOT NULL,
    phrase    TEXT NOT NULL,
    concept   TEXT NOT NULL
        REFERENCES pothos.concepts (label) ON DELETE CASCADE ON UPDATE CASCADE,
    PRIMARY KEY (dimension, phrase, concept),
    FOREIGN KEY (dimension, phrase)
        REFERENCES pothos.phrase_decisions (dimension, phrase) ON DELETE CASCADE
);
CREATE INDEX idx_phrase_concepts_concept ON pothos.phrase_concepts (concept);

-- Each user's concepts. An undecided phrase stands for itself; a phrase
-- decided as "no concept" drops out. is_current: the latest export carried it.
CREATE VIEW pothos.user_concepts AS
SELECT o.user_id, o.dimension,
       COALESCE(pc.concept, o.phrase)       AS concept,
       o.last_seen_at = m.engine_updated_at AS is_current
FROM pothos.observed_phrases o
JOIN pothos.insight_mirror m USING (user_id)
LEFT JOIN pothos.phrase_decisions d USING (dimension, phrase)
LEFT JOIN pothos.phrase_concepts pc USING (dimension, phrase)
WHERE d.phrase IS NULL OR pc.concept IS NOT NULL;

-- Supabase lockdown, as in 0001; ALL TABLES there only covered the tables of
-- that time. Guarded so plain Postgres (incl. the test DB) skips it.
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
ALTER TABLE pothos.observed_phrases ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.concepts         ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.phrase_decisions ENABLE ROW LEVEL SECURITY;
ALTER TABLE pothos.phrase_concepts  ENABLE ROW LEVEL SECURITY;
