//! Headless Chromium session lifecycle.
//!
//! Launches a chromium child process with `--remote-debugging-port=0`,
//! reads the DevTools WebSocket URL from its stderr, opens a fresh tab on
//! the Meet URL, runs the join flow, then hands the page session off to
//! the caption watcher.
//!
//! This mirrors `app/src-tauri/src/meet_call::meet_call_open_window` +
//! `meet_scanner` on the Shell side, except we own the browser process
//! ourselves rather than piggy-backing on CEF.
//!
//! ## Why not `chromiumoxide`?
//!
//! `chromiumoxide` is the obvious Rust Playwright client but adds ~30
//! transitive crates and historically lags chromium releases. We already
//! depend on `tokio-tungstenite` for the Shell-side CDP plumbing, so
//! driving CDP ourselves keeps the dependency surface flat and reuses
//! the same connection idioms the Shell uses (see
//! `app/src-tauri/src/cdp/conn.rs`).
//!
//! ## Process lifecycle
//!
//! 1. [`HeadlessSession::start`] spawns chromium and waits up to
//!    `CHROME_BOOT_BUDGET` for it to print its DevTools URL.
//! 2. The session is kept in a process-wide registry keyed by
//!    `request_id` so the matching `meet_headless_stop` RPC can find it.
//! 3. [`HeadlessSession::stop`] aborts the caption watcher, closes the
//!    browser, and removes the temporary user-data dir.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

use super::cdp::{result_str, CdpConn};
use super::{caption, join_flow};

const LOG_PREFIX: &str = "[meet-headless]";

/// Max wall-time to wait for chromium to print its DevTools URL after
/// launch. macOS cold starts are ~1.5 s; we give a generous 20 s.
const CHROME_BOOT_BUDGET: Duration = Duration::from_secs(20);

/// Where chromium prints the DevTools URL on stderr. Stable across
/// releases since Chrome 60-ish.
const DEVTOOLS_BANNER: &str = "DevTools listening on ";

/// Per-session bookkeeping. Owns the child process handle and the
/// caption watcher abort handle; dropping it must close both.
pub struct HeadlessSession {
    pub request_id: String,
    pub meet_url: String,
    pub display_name: String,
    /// `None` once the session has been stopped — guards against
    /// double-stop on a racing RPC.
    child: Option<Child>,
    user_data_dir: Option<PathBuf>,
    caption_shutdown: Option<oneshot::Sender<()>>,
}

/// Summary returned by `meet_headless_stop` for telemetry / smoke tests.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub request_id: String,
    pub captions_seen: u64,
}

struct HeadlessState {
    sessions: HashMap<String, HeadlessSession>,
}

static STATE: Lazy<Mutex<HeadlessState>> = Lazy::new(|| {
    Mutex::new(HeadlessState {
        sessions: HashMap::new(),
    })
});

impl HeadlessSession {
    /// Boot chromium, attach CDP, run the join flow, and start the
    /// caption watcher. Inserts the running session into the global
    /// registry on success — the caller does not need to.
    pub async fn start(
        meet_url: &str,
        display_name: &str,
        request_id: &str,
    ) -> Result<(), String> {
        // Refuse to overwrite a live session — the caller should stop
        // the existing one first. Mirrors the meet_agent registry's
        // behavior so RPC callers see a consistent surface.
        {
            let state = STATE.lock().expect("headless state poisoned");
            if state.sessions.contains_key(request_id) {
                return Err(format!("session already running: {request_id}"));
            }
        }

        let (mut child, ws_url, user_data_dir) = launch_chromium().await?;
        log::info!(
            "{LOG_PREFIX} chromium booted request_id={request_id} ws={ws_url} \
             user_data_dir={}",
            user_data_dir.display()
        );

        // Open a new tab on the Meet URL via the browser-level session,
        // then attach a flat session to it so we can drive Runtime.evaluate.
        let (mut cdp, session_id) = match open_meet_tab(&ws_url, meet_url).await {
            Ok(pair) => pair,
            Err(err) => {
                // Best-effort cleanup if attach failed mid-boot.
                let _ = child.kill().await;
                cleanup_user_data_dir(&user_data_dir);
                return Err(err);
            }
        };

        // Pre-enable the domains the join flow needs. Both calls are
        // idempotent and harmless if they fail — Runtime.evaluate works
        // without them on most chromium builds.
        let _ = cdp.call("Page.enable", Value::Null, Some(&session_id)).await;
        let _ = cdp
            .call("Runtime.enable", Value::Null, Some(&session_id))
            .await;

        if let Err(err) = join_flow::run(&mut cdp, &session_id, display_name).await {
            log::warn!(
                "{LOG_PREFIX} join flow incomplete request_id={request_id} err={err} \
                 — leaving session running so caller can recover manually"
            );
        }

        // Start the meet_agent session in-process (no RPC round-trip
        // needed — we're already inside Core). Same sample rate the
        // Shell side uses; Phase 6.3 will swap the speak-pump in.
        crate::openhuman::meet_agent::session::registry()
            .start(request_id, 16_000)
            .map_err(|e| format!("meet_agent.start_session: {e}"))?;

        // Spawn caption watcher with its own CDP connection so it can
        // drain captions concurrently with future audio bridge calls
        // (Phase 6.3). The CDP socket is single-reader, so the
        // caption watcher needs its own.
        let (caption_cdp, caption_session) = open_existing_page(&ws_url, meet_url).await?;
        let caption_shutdown =
            caption::spawn_watcher(request_id.to_string(), caption_cdp, caption_session);

        let session = HeadlessSession {
            request_id: request_id.to_string(),
            meet_url: meet_url.to_string(),
            display_name: display_name.to_string(),
            child: Some(child),
            user_data_dir: Some(user_data_dir),
            caption_shutdown: Some(caption_shutdown),
        };

        let mut state = STATE.lock().expect("headless state poisoned");
        state.sessions.insert(request_id.to_string(), session);
        Ok(())
    }

    /// Stop a session: shut down the caption watcher, the meet_agent
    /// session, the chromium process, and clean up the user-data dir.
    /// Safe to call once — a second call returns `not found`.
    pub async fn stop(request_id: &str) -> Result<SessionSummary, String> {
        let mut session = {
            let mut state = STATE.lock().expect("headless state poisoned");
            state
                .sessions
                .remove(request_id)
                .ok_or_else(|| format!("session not found: {request_id}"))?
        };

        // 1. Caption watcher first so it stops calling note_caption on
        //    the about-to-be-dropped meet_agent session.
        if let Some(tx) = session.caption_shutdown.take() {
            let _ = tx.send(());
        }

        // 2. Tear down the meet_agent session and capture its counters
        //    for the summary. If it's already gone (e.g. RPC stop ran
        //    first) we silently swallow — the summary just won't include
        //    those numbers.
        let _ =
            crate::openhuman::meet_agent::session::registry().stop(request_id);

        // 3. Kill chromium. `kill().await` is graceful on Unix (SIGKILL)
        //    and immediate on Windows; either way the OS reaps the child.
        if let Some(mut child) = session.child.take() {
            let _ = child.kill().await;
        }

        // 4. Best-effort cleanup of the ephemeral profile dir.
        if let Some(dir) = session.user_data_dir.take() {
            cleanup_user_data_dir(&dir);
        }

        Ok(SessionSummary {
            request_id: request_id.to_string(),
            captions_seen: caption::take_seen_count(request_id),
        })
    }

    /// Test helper — number of live sessions. Used by integration tests
    /// to confirm registry cleanup.
    #[allow(dead_code)]
    pub fn live_count() -> usize {
        STATE
            .lock()
            .expect("headless state poisoned")
            .sessions
            .len()
    }
}

/// Spawn chromium, parse its DevTools URL from stderr, return the child
/// handle so we can kill it later.
async fn launch_chromium() -> Result<(Child, String, PathBuf), String> {
    let exe = locate_chromium()?;

    // Ephemeral user-data dir so cookies, prefs, and the "do you want
    // to import?" first-run dialog don't bleed between calls.
    let user_data_dir = std::env::temp_dir().join(format!(
        "openhuman-meet-headless-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&user_data_dir)
        .map_err(|e| format!("create user_data_dir {}: {e}", user_data_dir.display()))?;

    let mut cmd = Command::new(&exe);
    cmd.arg("--headless=new")
        .arg("--no-sandbox")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        // Auto-accept getUserMedia prompts so the join flow doesn't stall
        // on the device-permission overlay.
        .arg("--use-fake-ui-for-media-stream")
        .arg("--use-fake-device-for-media-stream")
        // OS-picked port to avoid collisions with another live session.
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // chromium's signal-handling on Unix wants its own pgrp so
        // ctrl-c on the parent core doesn't cascade-kill it mid-call.
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn chromium at {}: {e}", exe.display()))?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "chromium stderr not captured".to_string())?;
    let ws_url = read_devtools_url(stderr).await?;
    Ok((child, ws_url, user_data_dir))
}

/// Block until chromium prints `DevTools listening on ws://...` or
/// [`CHROME_BOOT_BUDGET`] elapses. Logs other stderr lines at debug so a
/// failed boot leaves breadcrumbs.
async fn read_devtools_url(
    stderr: impl tokio::io::AsyncRead + Unpin,
) -> Result<String, String> {
    let mut reader = BufReader::new(stderr).lines();
    let deadline = Instant::now() + CHROME_BOOT_BUDGET;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("timeout waiting for chromium DevTools banner".into());
        }
        let line = match tokio::time::timeout(remaining, reader.next_line()).await {
            Ok(Ok(Some(l))) => l,
            Ok(Ok(None)) => return Err("chromium stderr closed before DevTools banner".into()),
            Ok(Err(e)) => return Err(format!("chromium stderr read: {e}")),
            Err(_) => return Err("timeout waiting for chromium DevTools banner".into()),
        };
        if let Some(url) = parse_devtools_line(&line) {
            return Ok(url);
        }
        log::debug!("{LOG_PREFIX} chromium stderr: {line}");
    }
}

/// Pull the `ws://...` URL out of a `DevTools listening on ws://...`
/// banner line. Returns `None` for any other line.
pub(super) fn parse_devtools_line(line: &str) -> Option<String> {
    let rest = line.strip_prefix(DEVTOOLS_BANNER)?;
    let url = rest.split_whitespace().next()?;
    if !url.starts_with("ws://") && !url.starts_with("wss://") {
        return None;
    }
    Some(url.to_string())
}

/// Locate a chromium / chrome binary on disk. Checks `$CHROME` and
/// `$CHROMIUM_PATH` first, then well-known platform locations.
pub(super) fn locate_chromium() -> Result<PathBuf, String> {
    for env_var in ["CHROME", "CHROMIUM_PATH", "PUPPETEER_EXECUTABLE_PATH"] {
        if let Ok(p) = std::env::var(env_var) {
            let path = PathBuf::from(p);
            if path.exists() {
                return Ok(path);
            }
        }
    }
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
    } else if cfg!(target_os = "linux") {
        &[
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
        ]
    } else if cfg!(target_os = "windows") {
        &[
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ]
    } else {
        &[]
    };
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(
        "could not locate chrome/chromium — set $CHROME to the binary path \
         (e.g. /Applications/Google Chrome.app/Contents/MacOS/Google Chrome)"
            .into(),
    )
}

/// Open the browser-level CDP socket, create a new tab pointing at
/// `meet_url`, then attach a flat session to it. Returns both the
/// connection (now multiplexed onto that session) and the session id.
async fn open_meet_tab(
    browser_ws_url: &str,
    meet_url: &str,
) -> Result<(CdpConn, String), String> {
    let mut cdp = CdpConn::open(browser_ws_url).await?;

    let result = cdp
        .call(
            "Target.createTarget",
            serde_json::json!({ "url": meet_url, "background": false }),
            None,
        )
        .await?;
    let target_id = result_str(&result, &["targetId"])
        .ok_or_else(|| format!("Target.createTarget missing targetId: {result}"))?;

    let attach = cdp
        .call(
            "Target.attachToTarget",
            serde_json::json!({ "targetId": target_id, "flatten": true }),
            None,
        )
        .await?;
    let session_id = result_str(&attach, &["sessionId"])
        .ok_or_else(|| format!("Target.attachToTarget missing sessionId: {attach}"))?;

    Ok((cdp, session_id))
}

/// Find the page target already pointing at `meet_url` (created by
/// [`open_meet_tab`]) and attach a *new* CDP socket to it. The caption
/// watcher uses this so its reads don't race with the join-flow socket.
async fn open_existing_page(
    browser_ws_url: &str,
    meet_url: &str,
) -> Result<(CdpConn, String), String> {
    let mut cdp = CdpConn::open(browser_ws_url).await?;

    // Poll Target.getTargets until the meet tab shows up — usually
    // instant since `Target.createTarget` already returned, but the
    // page-level target sometimes shows up a frame later.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut target_id: Option<String> = None;
    while Instant::now() < deadline {
        let res = cdp
            .call("Target.getTargets", Value::Null, None)
            .await?;
        if let Some(targets) = res.get("targetInfos").and_then(|v| v.as_array()) {
            for t in targets {
                let url = t.get("url").and_then(|v| v.as_str()).unwrap_or("");
                let ty = t.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if ty == "page" && url.starts_with(meet_url) {
                    target_id =
                        t.get("targetId").and_then(|v| v.as_str()).map(String::from);
                    break;
                }
            }
        }
        if target_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let target_id = target_id
        .ok_or_else(|| format!("no page target matching {meet_url} after Target.getTargets"))?;

    let attach = cdp
        .call(
            "Target.attachToTarget",
            serde_json::json!({ "targetId": target_id, "flatten": true }),
            None,
        )
        .await?;
    let session_id = result_str(&attach, &["sessionId"])
        .ok_or_else(|| format!("Target.attachToTarget missing sessionId: {attach}"))?;
    Ok((cdp, session_id))
}

fn cleanup_user_data_dir(dir: &PathBuf) {
    if let Err(err) = std::fs::remove_dir_all(dir) {
        log::debug!(
            "{LOG_PREFIX} failed to remove user_data_dir {}: {err}",
            dir.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devtools_banner() {
        let line = "DevTools listening on ws://127.0.0.1:54321/devtools/browser/abc-def";
        assert_eq!(
            parse_devtools_line(line).as_deref(),
            Some("ws://127.0.0.1:54321/devtools/browser/abc-def")
        );
    }

    #[test]
    fn rejects_non_banner_stderr() {
        assert_eq!(parse_devtools_line("Failed to launch GPU process"), None);
        assert_eq!(parse_devtools_line(""), None);
        // Mis-spelled banner — must not match.
        assert_eq!(
            parse_devtools_line("DevTools listening at ws://127.0.0.1:1/x"),
            None
        );
    }

    #[test]
    fn rejects_non_ws_scheme_banner() {
        // Defensive — if a future chromium ever prints http:// here we
        // want to fail loudly rather than try to open it as a WebSocket.
        let line = "DevTools listening on http://127.0.0.1:1/x";
        assert_eq!(parse_devtools_line(line), None);
    }
}
