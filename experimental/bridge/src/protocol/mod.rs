// `transport.rs` is kept byte-for-byte wire-compatible with the head unit's copy
// (`headunit/src/transport.rs`), so its full API is carried here even where the
// bridge only exercises the decode half. Suppress dead-code at the module level
// rather than annotating — and diverging — the shared file.
#[allow(dead_code)]
pub mod transport;
pub mod crypto;
pub mod control;
pub mod channels;
