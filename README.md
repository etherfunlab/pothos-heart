# pothos-heart

An open-source matchmaking engine. It reads what an AI companion has learned about a person and ranks the real people who could meet them — with the reason attached.

- **Hard filter in SQL**: bidirectional gender, bidirectional age window, bidirectional deal-breakers. Missing data never excludes anyone.
- **Profile merging**: every phrase the engine has ever exported is kept, and an LLM files each distinct phrase under a shared vocabulary of concepts, so "listens to old jazz" and "weekend jazz bars" meet at "jazz". Without a model configured, phrases match literally.
- **Soft score in Rust**: overlap coefficient per dimension over concepts (`|A∩B| / min(|A|,|B|)`), geo by level, weighted into `[0, 1]`. Every candidate comes back with the shared concepts that produced the score.
- **Nothing persisted about matches.** The ranking is computed on request. "Seen / skipped / matched" is the product layer's state.

Two inputs:

| Mirror | Source | How it arrives |
|---|---|---|
| `pothos.insight_mirror` + `pothos.observed_phrases` | an [eros-engine](https://github.com/etherfunlab/eros-engine) deployment's `GET /svc/v1/insights/changes` | pulled by the built-in sweeper |
| `pothos.match_profiles` | the product layer's self-filled profile (gender, who they seek, birth date, country / region / city) | pushed via `PUT /v1/profiles/{user_id}` |

The sweeper polls the engine every `SWEEP_INTERVAL_SECS`, resuming a few seconds behind the last row it stored (the engine stamps `updated_at` when a write starts but the row is visible only at commit). Re-fetched rows are idempotent upserts. Array items are stored trimmed and lowercase, one row per phrase.

## Profile merging

The engine overwrites a person's arrays wholesale on each extraction, so an item can vanish from one export to the next. `pothos.observed_phrases` keeps every phrase ever exported with `first_seen_at` / `last_seen_at`; a phrase is *current* when the latest export carried it.

After each sweep, the concept step sends undecided phrases to an OpenAI-compatible model, 40 per call and at most `CONCEPT_MAX_PER_TICK` per sweep, with the whole vocabulary in the prompt. For each phrase the model reuses a concept, coins one, or answers none. Decisions are global — a phrase is sent once, whoever wrote it — and record the model that made them; delete a `pothos.phrase_decisions` row to have that phrase decided again.

- Soft scoring uses every concept a person has ever had.
- Deal-breakers compare current concepts only, on both sides, so a phrase the engine has since dropped never excludes anyone.
- A phrase not yet decided takes part as itself; a phrase decided as "none" takes no part.

New concepts are written in `[concepts] label_language` from `pothos.toml` (default `en`). Run the sweeper on one instance only; set `SWEEP_DISABLED=1` on the others.

## Run it

```bash
docker run -d --name pothos-pg -p 5432:5432 -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=pothos postgres:16
cp .env.example .env            # fill `DATABASE_URL` and `POTHOS_CALLER_SECRET`; `ENGINE_*` only if you want the sweeper
cargo run -- migrate
cargo run -- serve
```

`migrate` must run on a direct connection, not through a transaction pooler — it sets `search_path` as a startup parameter so the migrator's bookkeeping table lands in the `pothos` schema.

`serve` must connect as the role that owns the `pothos` tables (the one that ran `migrate`): row-level security is enabled with no policies, so any other role sees no rows.

Without both `ENGINE_BASE_URL` and `ENGINE_EXPORT_SECRET` the sweeper stays off; setting only one refuses boot. The service then ranks whatever is already mirrored. `LLM_BASE_URL`, `LLM_API_KEY` and `LLM_MODEL` work the same way, all or none; the concept step runs only inside the sweeper.

The sweeper needs [eros-engine](https://github.com/etherfunlab/eros-engine) **v2.2.0 or later**, which serves `GET /svc/v1/insights/changes`, with `INSIGHTS_EXPORT_SECRET` set on the engine (at least 32 bytes) to the same value as `ENGINE_EXPORT_SECRET` here.

## API

Every `/v1/*` call needs `x-pothos-caller-secret: <POTHOS_CALLER_SECRET>`. This is a service-to-service API: end users never call it, and their identity system is not this service's business.

```
PUT    /v1/profiles/{user_id}             push hard attributes (creates pool membership)
DELETE /v1/profiles/{user_id}             leave the pool
GET    /v1/match/candidates/{user_id}     ranked candidates, limit clamped to 1..=100 (default 20)
GET    /healthz                           public
```

`PUT /v1/profiles/{user_id}` body:

```json
{ "gender": "f", "seeking_genders": ["m"], "birth_date": "1996-04-02",
  "country": "SG", "region": null, "city": "Singapore", "active": true }
```

`seeking_genders` is already expanded by the caller — this service never maps an orientation label to genders; that is a cultural and product decision, not an engine's. `gender` and `seeking_genders` are case-insensitive labels, stored lowercase. PUT replaces every field: an omitted optional field becomes null and an omitted `active` resets to `true`. Unknown keys are rejected (400).

`active: false` keeps the row but takes the user out of the pool: they drop out of everyone's candidates and get `404 not_in_pool` from `/v1/match/candidates`. `DELETE` of a non-member is also `404 not_in_pool`.

**Data lifecycle.** `DELETE /v1/profiles/{user_id}` removes only the hard attributes. The user's `pothos.insight_mirror` row and `pothos.observed_phrases` rows stay, and the export carries no deletions — to erase a user completely, also `DELETE FROM pothos.observed_phrases WHERE user_id = …` and `DELETE FROM pothos.insight_mirror WHERE user_id = …`. If the engine still holds that user's insights, its next export of them recreates both. Concept decisions are keyed by phrase text, shared by everyone who wrote it, and keep that text after one user's rows are gone.

`GET /v1/match/candidates/{user_id}` response:

```json
{
  "user_id": "…",
  "reason": null,
  "candidates": [
    { "user_id": "…", "score": 0.85,
      "scores": { "interests": 1.0, "personality_traits": 0.5, "geo": 1.0 },
      "shared_interests": ["hiking", "jazz"], "shared_traits": ["calm"], "geo_match": "city" }
  ]
}
```

`shared_interests` / `shared_traits` are concept labels, not either person's own words. `reason` is `"no_insights_yet"` (with an empty list) when the user is in the pool but the engine has not produced a profile for them yet. A user who was never pushed is `404 not_in_pool`.

Weights and the concept label language live in `pothos.toml`. The OpenAPI document: `cargo run -- print-openapi`.

## Configuration

See [`.env.example`](.env.example). `POTHOS_CALLER_SECRET` must be at least 32 bytes; a shorter one refuses boot.

## Development

```bash
createdb -h localhost -U postgres pothos_test
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/pothos_test
cargo test
cargo clippy --all-targets -- -D warnings
```

`cargo test` needs the database named in `DATABASE_URL` to exist (the `createdb` above); the harness creates throwaway databases on that server.

## License

MIT.
