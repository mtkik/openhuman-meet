//! Headless Meet Agent runner.
//!
//! Joins Google Meet via Playwright-style chromium control over CDP,
//! without requiring the Tauri desktop app or a CEF webview. The whole
//! browser lifecycle (launch, navigate, join flow, caption polling,
//! teardown) lives inside Core so OpenClaw skills and CLI callers can
//! drive Meet calls directly over the existing JSON-RPC surface.
//!
//! ## Phase scope (6.1 + 6.2)
//!
//! This module implements:
//! - **Runner**: chromium launch + CDP attach + session registry.
//! - **Join flow**: ports `meet_scanner` CDP automation.
//! - **Caption watcher**: ports `recipe.js` DOM scraping to a Rust
//!   poll loop that feeds the in-process `meet_agent` session.
//!
//! The **audio bridge** (`Phase 6.3`) and **fake camera** (`Phase 6.4`)
//! are not yet wired — `meet_headless_start` opens the call and the
//! agent can listen to captions, but cannot speak. The `meet_agent`
//! session it opens is the same shape the Shell-side runner uses, so
//! Phase 6.3 only needs to add the speak-pump glue.
//!
//! ## RPC surface
//!
//! - `openhuman.meet_headless_start` — launch chromium, join the call,
//!   start watching captions.
//! - `openhuman.meet_headless_stop`  — shut down the session.

pub mod caption;
pub mod cdp;
pub mod join_flow;
pub mod rpc;
pub mod runner;
pub mod schemas;

pub use runner::{HeadlessSession, SessionSummary};
pub use schemas::{
    all_controller_schemas as all_meet_headless_controller_schemas,
    all_registered_controllers as all_meet_headless_registered_controllers,
};
