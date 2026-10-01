//! Bakes `HS_VERSION` into the binary. The derivation (and its rules) live in
//! `build-support/version.rs`, shared by every crate that ships a binary.
#[path = "../../build-support/version.rs"]
mod version;

fn main() {
    version::emit();
}
