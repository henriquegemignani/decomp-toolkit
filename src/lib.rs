#![deny(unused_crate_dependencies)]
//! decomp-toolkit as a library.
//!
//! The `dtk` binary in `main.rs` is the primary consumer, and these modules are
//! arranged for it rather than as a curated public API. They are public so that
//! external tooling can reuse the DOL/REL analysis pipeline — loading a project
//! configuration, analysing an executable, reading and writing splits and
//! symbols — instead of shelling out to the binary and parsing its output.
//!
//! Nothing here is covered by a stability promise: it moves with `dtk`.

pub mod analysis;
pub mod argp_version;
pub mod cmd;
pub mod obj;
pub mod util;
pub mod vfs;

// Dependencies of the `dtk` binary alone. `unused_crate_dependencies` is
// checked per target, so without these the library target reports them unused.
use enable_ansi_support as _;
#[cfg(target_env = "musl")]
use mimalloc as _;
use supports_color as _;
use tracing_subscriber as _;
