//! Chain-agnostic building blocks shared by the `eth-copy-trade` and `sol-copy-trade`
//! binaries. Nothing here may depend on `eth` or `sol`.

pub mod backoff;
pub mod dedup;
pub mod env;
pub mod redact;
pub mod runtime;
pub mod telegram;
