//! The helpers that the tests of zoe-convert share.

use zoe_core::testing::TempDir;

/// Return a new, empty directory for the test `name`. The guard removes
/// the directory when the test passes, and keeps it when the test fails.
pub fn fresh_dir(name: &str) -> TempDir {
    zoe_core::testing::fresh_dir(&format!("convert-{}-{name}", env!("CARGO_CRATE_NAME")))
}
