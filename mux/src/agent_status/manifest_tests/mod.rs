//! Tests for individual agent manifests.
//!
//! Each file here covers one bundled manifest. They stay out of
//! [`engine`](crate::agent_status::engine) so a new agent adds a file here
//! instead of growing the engine's generic test module.

mod codebuddy;
mod omp;
