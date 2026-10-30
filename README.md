# pothos-heart

An open-source matchmaking engine. It reads what an AI companion has learned about a person and ranks the real people who could meet them — with the reason attached.

- **Hard filter in SQL**: bidirectional gender, bidirectional age window, bidirectional deal-breakers. Missing data never excludes anyone.
- **Profile merging**: every phrase the engine has ever exported is kept, and an LLM files each distinct phrase under a shared vocabulary of concepts, so "listens to old jazz" and "weekend jazz bars" meet at "jazz". Without a model configured, phrases match literally.
- **Soft score in Rust**: overlap coefficient per dimension over concepts (`|A∩B| / min(|A|,|B|)`), geo by level, weighted into `[0, 1]`. Every candidate comes back with the shared concepts that produced the score.
- **Autobio and judge**: each pool member gets an autobio, short dated statements with their sources — the engine's ledger facts and extracted memories — kept up to date by an LLM. A judge reads two autobios and grades the pair; the grade reranks the candidate list.
- **Nothing persisted about matches.** The ranking is computed on request; "seen / skipped / matched" is the product layer's state. The judge's grades are cached per pair and per autobio version.

Two inputs:

| Mirror | Source | How it arrives |
|---|---|---|
| `pothos.insight_mirror` + `pothos.observed_phrases`, `pothos.memory_mirror`, `pothos.fact_mirror` | an [eros-engine](https://github.com/etherfunlab/eros-engine) deployment's `GET /svc/v1/insights/changes`, `GET /svc/v1/memories/changes` and `GET /svc/v1/facts/changes` | pulled by the built-in sweeper |
| `pothos.match_profiles` | the product layer's self-filled profile (gender, who they seek, birth date, country / region / city, age window) | pushed via `PUT /v1/profiles/{user_id}` |

The sweeper polls each export every `SWEEP_INTERVAL_SECS`, resuming a few seconds behind the last row it stored (the engine stamps a row's time when a write starts but the row is visible only at commit). Re-fetched rows are idempotent upserts. Array items are stored trimmed, lowercase and cut to 2000 bytes, one row per phrase; the export's text columns are derived summaries of the fact ledger and are not read. A fact re-seen by the engine updates its mirrored row and counts as new autobio material; an unchanged re-fetch does not.

Each sweep starts with the engine's deletion stream (`GET /svc/v1/insights/deletions`): a user the engine erased loses their mirrored rows and everything derived here — autobio entries (deleted ones included), autobio state, verdicts and queued pairs. Their `match_profiles` row is the product layer's and stays; see Data lifecycle.

## Profile merging

The engine overwrites a person's arrays wholesale on each extraction, so an item can vanish from one export to the next. `pothos.observed_phrases` keeps every array phrase ever exported, with `first_seen_at` / `last_seen_at`; a row is *current* when the latest export carried it. A phrase is cut to at most 2000 bytes at a character boundary before it enters the ledger, which keeps a row under Postgres's btree entry limit; a cut is logged without the text.

After each sweep, the concept step sends undecided phrases to an OpenAI-compatible model, 40 per call and at most `CONCEPT_MAX_PER_TICK` per sweep, with the whole vocabulary in the prompt. For each phrase the model reuses a concept, coins one, or answers none. Decisions are global — a phrase is sent once, whoever wrote it — and record the model that made them; delete a `pothos.phrase_decisions` row to have that phrase decided again.

- Soft scoring uses every concept a person has ever had.
- Deal-breakers compare current concepts only, on both sides, so a phrase the engine has since dropped never excludes anyone.
- A phrase not yet decided takes part as itself; a phrase decided as "none" takes no part.

New concepts are written in `[concepts] label_language` from `pothos.toml` (default `en`). Run the sweeper on one instance only; set `SWEEP_DISABLED=1` on the others.

## Autobio

Every user with a `match_profiles` row (any `active`) gets an autobio. Each entry is one statement under a fixed section (`basics`, `life`, `personality`, `love`, `intimacy`, `outlook`, `roleplay`), with a status (`current`, `past`, `planned`), a time anchor and the material it cites.

The autobio step runs inside the sweeper, after the concept step. Each user who has new material, and has not been updated in the last `AUTOBIO_MIN_INTERVAL_SECS`, gets one model call. The call carries their current entries, the material recorded since their last run — ledger facts with their extraction labels and recurrence, and extracted memories — and the statements they deleted. A fact the engine saw again arrives marked as such, to confirm an entry rather than duplicate it. The model answers with add / revise / retire operations, each checked on its own. Revising retires the old entry and adds a new one that points back, so retired entries are the history. Role-play material is kept in its own section. A request the provider refuses (a 4xx such as moderation) skips that user until the interval has passed and the step moves on to the next; when the provider is unreachable or turns every request away (a timeout, 401, 402, 429, or a 5xx after two retries), the step stops for that tick.

- The owner reads it with `GET /v1/autobio/{user_id}` and deletes an entry with `DELETE /v1/autobio/{user_id}/entries/{entry_id}`. A deleted entry is gone at once and the model is told never to write it back.
- To rebuild one user after changing models or prompts: `DELETE FROM pothos.autobio_entries WHERE user_id = … AND retired_reason IS DISTINCT FROM 'vetoed'` and `DELETE FROM pothos.autobio_state WHERE user_id = …`; the next sweep rewrites it from all material.

## Judge

`GET /v1/match/candidates/{user_id}` queues the pairs among the top `JUDGE_TOP_K` candidates that lack a fresh grade, provided both users have an autobio. The sweeper's judge step reads both autobios and grades the pair `strong`, `promising`, `unclear` or `poor`. A request the provider refuses drops that pair until a later candidates request queues it again, and the step moves on; when the provider is unreachable or turns every request away, the step stops for that tick, the pairs stay queued, and the pair that failed moves to the back of the queue so it does not head every tick. The grade is cached against both autobio versions, and a later change to either one re-queues the pair on the next request; until it is judged again the old grade stays on the candidate. Candidates carry the grade as `chemistry` and are reordered by it: strong, promising, then unjudged and unclear by score, poor last. Nobody is dropped. Without a model configured nothing is queued and `chemistry` is always `null`. That holds per instance: an instance that serves `/v1/match/candidates` shows `chemistry` only when `LLM_*` is set on it, even one running with `SWEEP_DISABLED=1`.

The judge's rationale is stored for the maintainers and never returned by the API: it quotes both autobios, and an autobio is for its owner only.

## Run it

```bash
docker run -d --name pothos-pg -p 5432:5432 -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=pothos postgres:16
cp .env.example .env            # fill `DATABASE_URL` and `POTHOS_CALLER_SECRET`; `ENGINE_*` only if you want the sweeper
cargo run -- migrate
cargo run -- serve
```

`migrate` must run on a direct connection, not through a transaction pooler — it sets `search_path` as a startup parameter so the migrator's bookkeeping table lands in the `pothos` schema.

`serve` must connect as the role that owns the `pothos` tables (the one that ran `migrate`): row-level security is enabled with no policies, so any other role sees no rows.

Without both `ENGINE_BASE_URL` and `ENGINE_EXPORT_SECRET` the sweeper stays off; setting only one refuses boot, as does `ENGINE_FACTS_SECRET` without the pair. The service then ranks whatever is already mirrored. `LLM_BASE_URL`, `LLM_API_KEY` and `LLM_MODEL` work the same way, all or none; the concept, autobio and judge steps run only inside the sweeper. `AUTOBIO_MODEL` and `JUDGE_MODEL` replace the model name for their step. Each attempt waits up to 300 s for an autobio or judge reply and 60 s for a concept reply.

The sweeper needs [eros-engine](https://github.com/etherfunlab/eros-engine) **v2.4.0 or later** for the insights export and the deletion stream, with `INSIGHTS_EXPORT_SECRET` set on the engine (at least 32 bytes) to the same value as `ENGINE_EXPORT_SECRET` here. The facts export is a second credential: `FACTS_EXPORT_SECRET` on the engine, `ENGINE_FACTS_SECRET` here. It is optional — without it (or against an engine that answers 404) the facts step is skipped and autobio runs on the memory material alone, as it does when the memories export is missing the other way around.

## API

Every `/v1/*` call needs `x-pothos-caller-secret: <POTHOS_CALLER_SECRET>`. This is a service-to-service API: end users never call it, and their identity system is not this service's business.

```
PUT    /v1/profiles/{user_id}                      push hard attributes (creates pool membership)
DELETE /v1/profiles/{user_id}                      leave the pool
GET    /v1/match/candidates/{user_id}              ranked candidates, limit clamped to 1..=100 (default 20)
GET    /v1/autobio/{user_id}                       the user's current autobio, by section
DELETE /v1/autobio/{user_id}/entries/{entry_id}    delete one entry
GET    /healthz                                    public
```

`PUT /v1/profiles/{user_id}` body:

```json
{ "gender": "f", "seeking_genders": ["m"], "birth_date": "1996-04-02",
  "country": "SG", "region": null, "city": "Singapore",
  "age_min": 25, "age_max": 40, "active": true }
```

`seeking_genders` is already expanded by the caller — this service never maps an orientation label to genders; that is a cultural and product decision, not an engine's. `gender` and `seeking_genders` are case-insensitive labels, stored lowercase. `age_min` / `age_max` are the user's own age window for candidates, applied both ways in the hard filter; either side may stand alone, each must be within 1–130 and `age_min` must not exceed `age_max` (400 otherwise). PUT replaces every field: an omitted optional field becomes null and an omitted `active` resets to `true`. Unknown keys are rejected (400).

`active: false` keeps the row but takes the user out of the pool: they drop out of everyone's candidates and get `404 not_in_pool` from `/v1/match/candidates`. `DELETE` of a non-member is also `404 not_in_pool`.

**Data lifecycle.** `DELETE /v1/profiles/{user_id}` removes the hard attributes, the user's autobio and its state, and every verdict or queued pair naming them; the mirrored engine data stays. The engine's deletion stream covers the other half: when the engine erases a user (`POST /svc/v1/users/{user_id}/erase` there), the sweeper drops their `insight_mirror`, `observed_phrases`, `memory_mirror` and `fact_mirror` rows and everything derived from them — autobio entries (deleted ones included), autobio state, verdicts and queued pairs — while the `match_profiles` row, being the product layer's, stays. So a product layer deleting an account calls both: the engine's erase endpoint and `DELETE /v1/profiles/{user_id}` here, in either order. A user who joins again gets an autobio written from whatever material the mirrors still hold. Both steps check pool membership again right before each model call, so a user who has already left is not sent to the model. An autobio or judge write for a user who leaves while the model is answering is discarded: both check pool membership under a row lock in the same transaction, so that leave leaves nothing behind. Queueing a pair takes the same lock on both profile rows, so a candidates request that races a leave cannot queue a pair naming the user who left. Concept decisions are keyed by phrase text, shared by everyone who wrote it, and keep that text after one user's rows are gone.

`GET /v1/match/candidates/{user_id}` response:

```json
{
  "user_id": "…",
  "reason": null,
  "candidates": [
    { "user_id": "…", "score": 0.85,
      "scores": { "interests": 1.0, "personality_traits": 0.5, "geo": 1.0 },
      "shared_interests": ["hiking", "jazz"], "shared_traits": ["calm"], "geo_match": "city",
      "chemistry": "promising" }
  ]
}
```

`shared_interests` / `shared_traits` are concept labels, not either person's own words. `chemistry` is the judge's grade, `null` until judged. `reason` is `"no_insights_yet"` (with an empty list) when the user is in the pool but the engine has not produced a profile for them yet. A user who was never pushed is `404 not_in_pool`.

`GET /v1/autobio/{user_id}` lists `sections` in the order given under Autobio, empty ones left out; each entry carries `id`, `statement`, `status` and `as_of`, the time its latest source was first said. `updated_at` is the autobio's version, and `reason` is `"no_autobio_yet"` while no entry stands. Any `match_profiles` row may read it, `active: false` included, so a user can preview it before joining; no row is `404 not_in_pool`. An entry that is not the user's, or no longer current, is `404 entry_not_found`. An autobio is for its owner only: making sure the caller is the owner is the product layer's job.

Weights and the language settings live in `pothos.toml`. The OpenAPI document: `cargo run -- print-openapi`.

## Configuration

See [`.env.example`](.env.example): the `AUTOBIO_*` and `JUDGE_*` settings are there beside `CONCEPT_MAX_PER_TICK`; `AUTOBIO_MAX_PER_TICK=0` or `JUDGE_MAX_PER_TICK=0` turns that step off. `POTHOS_CALLER_SECRET` must be at least 32 bytes; a shorter one refuses boot. `pothos.toml` holds the weights, `[concepts] label_language` and `[autobio] language` (the language of autobio entries and judge rationales, default `en`).

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
