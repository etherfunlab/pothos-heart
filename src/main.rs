//! pothos-heart — an open-source matchmaking engine.
//!
//! Subcommands: `serve` (default) · `migrate` · `print-openapi`.

#[allow(dead_code)]
mod routes;

fn main() {
    println!("pothos-heart {}", env!("CARGO_PKG_VERSION"));
}
