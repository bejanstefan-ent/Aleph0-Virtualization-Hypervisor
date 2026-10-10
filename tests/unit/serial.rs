//! Unit tests for `src/serial.rs`.
//!
//! Compiled only by `cargo test-host`. That file includes this one with
//! `#[path]` as its child module `tests`, so `use super::*` reaches its
//! private items.

use super::*;

#[test]
fn divisor_for_common_rates() {
    assert_eq!(divisor(115_200), 1);
    assert_eq!(divisor(57_600), 2);
    assert_eq!(divisor(9_600), 12);
}
