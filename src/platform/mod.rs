//! Everything that talks to macOS itself (mount table, `diskutil`). The rest of
//! the crate sees only the types in `devices`.

pub mod macos;
