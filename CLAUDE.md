# pothos-heart

Open-source matchmaking engine (MIT). Companion of `eros-engine`; the two talk HTTP only.

## Rules

- **No `eros_engine_*` crate dependency, ever.** The engine is AGPL-3.0; linking it would make the MIT license untrue. The two services talk HTTP only.
- **Never read another schema.** `pothos.*` is the whole world. Hard attributes arrive by `PUT /v1/profiles`, soft signals by the engine's export. No `web.*`, no `engine.*`, no `auth.*`.
- **Public repo.** Issues, PRs, comments and docs say "a product layer" / "a downstream deployment". Never the web repo's name, a production URL, or a private repo path — except in a spec on a feature branch (next rule).
- **Every feature gets its own branch, squash-merged into `main`.** No PRs before v1.0. A spec in `docs/superpowers/specs/` rides the branch as written and may name the web repo; once the work is built and codex-reviewed, rewrite it neutrally, then `git merge --squash`, so `main`'s history never names it. Plans in `docs/superpowers/plans/` stay untracked.

## Commands

`cargo run -- migrate` · `cargo run -- serve` · `cargo run -- print-openapi` · `cargo test` (needs `DATABASE_URL`)
