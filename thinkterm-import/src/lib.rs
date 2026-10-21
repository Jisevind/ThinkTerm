//! Source-independent session import plans and native adapter registration.
//! The wire model also builds for wasm; adapters never depend on the mux.
mod model;
pub use model::*;

#[cfg(any(unix, windows))]
mod source;
#[cfg(any(unix, windows))]
pub use source::*;

#[cfg(test)]
mod tests;
