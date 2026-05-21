//! Headless Meet Agent runner.
//!
//! Joins Google Meet via Playwright-style chromium control over CDP,
//! without requiring the Tauri desktop app or a CEF webview. The whole
//! browser lifecycle (launch, navigate, join flow, caption polling,
//! audio bridge, fake camera, teardown) lives inside Core so OpenClaw
//! skills and CLI callers can drive Meet calls directly over the
//! existing JSON-RPC surface.
//!
//! ## Phase scope (6.1–6.4)
//!
//! This module implements:
//! - **Runner**: chromium launch + CDP attach + session registry.
//! - **Join flow**: ports `meet_scanner` CDP automation.
//! - **Caption watcher**: ports `recipe.js` DOM scraping to a Rust
//!   poll loop that feeds the in-process `meet_agent` session.
//! - **Audio bridge**: bidirectional PCM16LE audio between the page
//!   and Core's `meet_agent` (capture + playback).
//! - **Fake camera**: overrides `getUserMedia` to return a canvas-based
//!   video stream and silent audio stream.
//!
//! ## RPC surface
//!
//! - `openhuman.meet_headless_start` — launch chromium, join the call,
//!   start watching captions, start audio bridge.
//! - `openhuman.meet_headless_stop`  — shut down the session.

pub mod audio_bridge;
pub mod caption;
pub mod cdp;
pub mod fake_camera;
pub mod join_flow;
pub mod rpc;
pub mod runner;
pub mod schemas;

pub use runner::{HeadlessSession, SessionSummary};
pub use schemas::{
    all_controller_schemas as all_meet_headless_controller_schemas,
    all_registered_controllers as all_meet_headless_registered_controllers,
};
