//! MultiPlex client library: controller connection, media dispatch, status UI.
//! The `multiplex-client` binary is a thin wrapper; the split exists so
//! integration tests can exercise the connection logic directly.

pub mod connection;
pub mod discovery;
pub mod identity;
pub mod state;
pub mod ui;
pub mod update;
