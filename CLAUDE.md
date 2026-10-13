# pothos-heart

Open-source matchmaking engine (MIT). Companion of `eros-engine`; shares a tree with it under `oss-eros/`, inherits that tree's `CLAUDE.md`.

## Three rules

- **No `eros_engine_*` crate dependency, ever.** The engine is AGPL-3.0; linking it would make the MIT license untrue. The two services talk HTTP only.
- **Never read another schema.** `pothos.*` is the whole world. Hard attributes arrive by `PUT /v1/profiles`, soft signals by the engine's export. No `web.*`, no `engine.*`, no `auth.*`.
- **Public repo.** Issues, PRs, comments and docs say "a product layer" / "a downstream deployment". Never the web repo's name, a production URL, or a private repo path.

## Commands

`cargo run -- migrate` · `cargo run -- serve` · `cargo run -- print-openapi` · `cargo test` (needs `DATABASE_URL`)
