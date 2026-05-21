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
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex};

use super::cdp::{result_str, CdpConn};
use super::{audio_bridge, caption, fake_camera, join_flow};

const LOG_PREFIX: &str = "[meet-headless]";

/// Max wall-time to wait for chromium to print its DevTools URL after
/// launch. macOS cold starts are ~1.5 s; we give a generous 20 s.
const CHROME_BOOT_BUDGET: Duration = Duration::from_secs(20);

/// Where chromium prints the DevTools URL on stderr. Stable across
/// releases since Chrome 60-ish.
const DEVTOOLS_BANNER: &str = "DevTools listening on ";

/// Filename prefix used for the per-session ephemeral user-data dir.
/// Shared with [`cleanup_stale_profiles`] so we can sweep up dirs left
/// behind by a previous Core crash on next launch.
const PROFILE_PREFIX: &str = "openhuman-meet-headless-";

/// Per-session bookkeeping. Owns the child process handle and the
/// caption watcher + audio bridge abort handles; dropping it must close
/// all of them.
pub struct HeadlessSession {
    pub request_id: String,
    pub meet_url: String,
    pub display_name: String,
    /// `None` once the session has been stopped — guards against
    /// double-stop on a racing RPC.
    child: Option<Child>,
    user_data_dir: Option<PathBuf>,
    caption_shutdown: Option<oneshot::Sender<()>>,
    audio_shutdown: Option<oneshot::Sender<()>>,
}

/// Summary returned by `meet_headless_stop` for telemetry / smoke tests.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub request_id: String,
    pub captions_seen: u64,
}

/// Registry slot per `request_id`. `Pending` reserves the slot while
/// chromium is booting so a concurrent `start("same-id")` can't pass the
/// existence check and clobber the in-flight launch.
enum SessionEntry {
    Pending,
    Active(HeadlessSession),
}

struct HeadlessState {
    sessions: HashMap<String, SessionEntry>,
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
        // Reserve the slot atomically so two concurrent `start(same-id)`
        // calls can't both pass the contains_key check, race through
        // chromium boot, and end up overwriting each other in the map.
        {
            let mut state = STATE.lock().await;
            if state.sessions.contains_key(request_id) {
                return Err(format!("session already running: {request_id}"));
            }
            state
                .sessions
                .insert(request_id.to_string(), SessionEntry::Pending);
        }

        match Self::start_session_inner(meet_url, display_name, request_id).await {
            Ok(session) => {
                let mut state = STATE.lock().await;
                // If a stop() raced in and removed the Pending slot,
                // dropping `session` here will tear down the browser and
                // signal the caption watcher via its Drop impl.
                match state.sessions.get(request_id) {
                    Some(SessionEntry::Pending) => {
                        state
                            .sessions
                            .insert(request_id.to_string(), SessionEntry::Active(session));
                        Ok(())
                    }
                    _ => {
                        let _ = crate::openhuman::meet_agent::session::registry()
                            .stop(request_id);
                        Err(format!("session {request_id} was cancelled during start"))
                    }
                }
            }
            Err(err) => {
                let mut state = STATE.lock().await;
                state.sessions.remove(request_id);
                Err(err)
            }
        }
    }

    /// Build the [`HeadlessSession`] — split out so the outer `start`
    /// can wrap any failure with a single Pending-slot cleanup. The
    /// returned session is "live but not registered"; on early Err the
    /// session's Drop will release chromium and the profile dir, and
    /// this fn explicitly tears down any meet_agent registration it
    /// made (the agent registry lives in a separate module so Drop on
    /// HeadlessSession alone can't reach it).
    async fn start_session_inner(
        meet_url: &str,
        display_name: &str,
        request_id: &str,
    ) -> Result<Self, String> {
        cleanup_stale_profiles();

        let (child, ws_url, user_data_dir) = launch_chromium().await?;
        log::info!(
            "{LOG_PREFIX} chromium booted request_id={request_id} ws={} \
             user_data_dir={} display_name.len={}",
            redact_ws_url(&ws_url),
            user_data_dir.display(),
            display_name.len()
        );

        // Wrap the resources in a HeadlessSession immediately so any
        // early return below drops them and cleans up via the Drop impl
        // (kill_on_drop handles chromium, Drop handles profile dir).
        let mut session = HeadlessSession {
            request_id: request_id.to_string(),
            meet_url: meet_url.to_string(),
            display_name: display_name.to_string(),
            child: Some(child),
            user_data_dir: Some(user_data_dir),
            caption_shutdown: None,
            audio_shutdown: None,
        };

        // Open a new tab on the Meet URL via the browser-level session,
        // then attach a flat session to it so we can drive Runtime.evaluate.
        let (mut cdp, session_id) = open_meet_tab(&ws_url, meet_url).await?;

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
        // caption watcher needs its own. If opening the second page
        // fails, the meet_agent session we just started above would
        // leak — tear it down explicitly before bailing.
        let (caption_cdp, caption_session) = match open_existing_page(&ws_url, meet_url).await
        {
            Ok(pair) => pair,
            Err(err) => {
                let _ =
                    crate::openhuman::meet_agent::session::registry().stop(request_id);
                return Err(format!("open caption page: {err}"));
            }
        };

        let caption_shutdown =
            caption::spawn_watcher(request_id.to_string(), caption_cdp, caption_session)
                .await;
        session.caption_shutdown = Some(caption_shutdown);

        // Phase 6.3 — Audio bridge. Needs its own CDP connection (third
        // socket) so its reads/writes don't race with the caption watcher
        // or join flow. Injection and bridge spawn are best-effort: if the
        // page doesn't support the Web Audio API (e.g. very old Chromium)
        // we log and continue — captions still work.
        let (audio_cdp, audio_session) = match open_existing_page(&ws_url, meet_url).await {
            Ok(pair) => pair,
            Err(err) => {
                log::warn!(
                    "{LOG_PREFIX} audio bridge: failed to open CDP connection: {err} \
                     — audio bridge disabled"
                );
                return Ok(session);
            }
        };

        let mut audio_cdp = audio_cdp;
        if let Err(err) =
            audio_bridge::inject_audio_scripts(&mut audio_cdp, &audio_session).await
        {
            log::warn!(
                "{LOG_PREFIX} audio bridge injection failed: {err} \
                 — audio bridge disabled"
            );
        } else {
            let audio_shutdown = audio_bridge::spawn_audio_bridge(
                request_id.to_string(),
                audio_cdp,
                audio_session,
            )
            .await;
            session.audio_shutdown = Some(audio_shutdown);
            log::info!(
                "{LOG_PREFIX} audio bridge started request_id={request_id}"
            );
        }

        Ok(session)
    }

    /// Stop a session: shut down the caption watcher, the meet_agent
    /// session, the chromium process, and clean up the user-data dir.
    /// Safe to call once — a second call returns `not found`.
    pub async fn stop(request_id: &str) -> Result<SessionSummary, String> {
        let mut session = {
            let mut state = STATE.lock().await;
            match state.sessions.get(request_id) {
                None => return Err(format!("session not found: {request_id}")),
                Some(SessionEntry::Pending) => {
                    // Don't yank the slot from under the in-flight
                    // start() — the caller can retry once it transitions
                    // to Active.
                    return Err(format!(
                        "session {request_id} is still starting; retry shortly"
                    ));
                }
                Some(SessionEntry::Active(_)) => match state.sessions.remove(request_id) {
                    Some(SessionEntry::Active(s)) => s,
                    _ => unreachable!("entry was Active when we peeked"),
                },
            }
        };

        // 1. Caption watcher first so it stops calling note_caption on
        //    the about-to-be-dropped meet_agent session.
        if let Some(tx) = session.caption_shutdown.take() {
            let _ = tx.send(());
        }

        // 1b. Audio bridge — same pattern, stop before meet_agent teardown.
        if let Some(tx) = session.audio_shutdown.take() {
            let _ = tx.send(());
        }

        // 2. Tear down the meet_agent session and capture its counters
        //    for the summary. If it's already gone (e.g. RPC stop ran
        //    first) we silently swallow — the summary just won't include
        //    those numbers.
        let _ = crate::openhuman::meet_agent::session::registry().stop(request_id);

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
            captions_seen: caption::take_seen_count(request_id).await,
        })
    }

    /// Test helper — number of live sessions. Used by integration tests
    /// to confirm registry cleanup.
    #[allow(dead_code)]
    pub async fn live_count() -> usize {
        STATE.lock().await.sessions.len()
    }
}

impl Drop for HeadlessSession {
    fn drop(&mut self) {
        // oneshot::Sender::send is sync — safe in Drop. If start()
        // succeeded normally and the session was later stopped via
        // stop(), all of these fields are already None and this is a
        // no-op; this Drop is the safety net for the *abnormal* paths
        // (panic, early-return error during start, parent task abort).
        if let Some(tx) = self.caption_shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(tx) = self.audio_shutdown.take() {
            let _ = tx.send(());
        }
        // kill_on_drop(true) on the Child handle takes care of the
        // chromium process. Nothing to do here for `self.child`.
        if let Some(dir) = self.user_data_dir.take() {
            if let Err(err) = std::fs::remove_dir_all(&dir) {
                log::debug!(
                    "{LOG_PREFIX} drop: failed to remove user_data_dir {}: {err}",
                    dir.display()
                );
            }
        }
    }
}

/// Sweep stray ephemeral profile directories left over from a previous
/// Core crash. Same prefix as [`launch_chromium`]'s temp dir naming, so
/// dirs from previous runs of the *same* code get caught — but no other
/// `/tmp` content is touched. Best-effort; failures are logged at debug.
pub fn cleanup_stale_profiles() {
    let tmp = std::env::temp_dir();
    let entries = match std::fs::read_dir(&tmp) {
        Ok(e) => e,
        Err(err) => {
            log::debug!(
                "{LOG_PREFIX} cleanup_stale_profiles: read_dir {} err={err}",
                tmp.display()
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(PROFILE_PREFIX) {
            continue;
        }
        let path = entry.path();
        if let Err(err) = std::fs::remove_dir_all(&path) {
            log::debug!(
                "{LOG_PREFIX} cleanup_stale_profiles: remove {} err={err}",
                path.display()
            );
        }
    }
}

/// Strip the per-launch UUID token from a DevTools WebSocket URL before
/// logging. The token grants full Runtime.evaluate access to the live
/// Meet session, so an info-level log leak is effectively an RCE leak.
pub fn redact_ws_url(url: &str) -> String {
    match url.rfind('/') {
        Some(idx) => format!("{}*****", &url[..=idx]),
        None => "*****".to_string(),
    }
}

/// Spawn chromium, parse its DevTools URL from stderr, return the child
/// handle so we can kill it later.
async fn launch_chromium() -> Result<(Child, String, PathBuf), String> {
    let exe = locate_chromium()?;

    // Ephemeral user-data dir so cookies, prefs, and the "do you want
    // to import?" first-run dialog don't bleed between calls.
    let user_data_dir = std::env::temp_dir().join(format!(
        "{PROFILE_PREFIX}{}",
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
pub fn parse_devtools_line(line: &str) -> Option<String> {
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

/// Open the browser-level CDP socket, create a new tab, inject the
/// fake-camera override on the **target-level** session, then navigate
/// to `meet_url`. Returns both the connection (multiplexed onto that
/// session) and the session id.
///
/// The fake-camera override is injected via
/// `Page.addScriptToEvaluateOnNewDocument` on the target session (not
/// the browser session) so it runs before any page JS. The tab is first
/// created with `about:blank` to get a target session, then the script
/// is injected, and finally the page navigates to the Meet URL.
async fn open_meet_tab(
    browser_ws_url: &str,
    meet_url: &str,
) -> Result<(CdpConn, String), String> {
    let mut cdp = CdpConn::open(browser_ws_url).await?;

    // Step 1: Create a new tab with about:blank so we get a targetId.
    let result = cdp
        .call(
            "Target.createTarget",
            serde_json::json!({ "url": "about:blank" }),
            None,
        )
        .await?;
    let target_id = result_str(&result, &["targetId"])
        .ok_or_else(|| format!("Target.createTarget missing targetId: {result}"))?;

    // Step 2: Attach to the target to get a page-level session.
    let attach = cdp
        .call(
            "Target.attachToTarget",
            serde_json::json!({ "targetId": target_id, "flatten": true }),
            None,
        )
        .await?;
    let session_id = result_str(&attach, &["sessionId"])
        .ok_or_else(|| format!("Target.attachToTarget missing sessionId: {attach}"))?;

    // Step 3: Enable Page domain on the target session.
    let _ = cdp
        .call("Page.enable", Value::Null, Some(&session_id))
        .await;

    // Step 4: Inject fake-camera override on the target session so it
    // runs before any page JS on subsequent navigations.
    if let Err(err) =
        fake_camera::inject_fake_camera(&mut cdp, Some(&session_id)).await
    {
        log::warn!(
            "{LOG_PREFIX} fake-camera injection failed (will use Chrome flags): {err}"
        );
    }

    // Step 5: Navigate to the actual Meet URL. The fake-camera override
    // is already active via addScriptToEvaluateOnNewDocument.
    let _ = cdp
        .call(
            "Page.navigate",
            serde_json::json!({ "url": meet_url }),
            Some(&session_id),
        )
        .await;

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

fn cleanup_user_data_dir(dir: &Path) {
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

    #[test]
    fn redacts_devtools_uuid_from_ws_url() {
        let url = "ws://127.0.0.1:54321/devtools/browser/3f4e2b1a-aaaa-bbbb-cccc-deadbeefcafe";
        let redacted = redact_ws_url(url);
        assert_eq!(
            redacted,
            "ws://127.0.0.1:54321/devtools/browser/*****"
        );
        assert!(!redacted.contains("deadbeef"));
    }

    #[test]
    fn redact_handles_url_without_slash() {
        // Defensive — never echo the raw input in the fallback case.
        assert_eq!(redact_ws_url("not-a-url"), "*****");
    }
}
