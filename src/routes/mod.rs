//! Route composition. Two sub-trees: public `/healthz`, and `/v1/*` behind the
//! caller-secret layer (added in a later task).

pub mod health;
