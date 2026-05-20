//! CDP-driven Meet join automation for the headless runner.
//!
//! Ported from `app/src-tauri/src/meet_scanner/mod.rs`. Three phases:
//!
//!  1. Dismiss the device-check ("Continue without microphone and camera")
//!     — usually skipped because we launch chromium with
//!     `--use-fake-ui-for-media-stream`, but we keep the phase as a
//!     best-effort probe in case a future build runs without that flag.
//!  2. Type the supplied guest display name into the "Your name" input
//!     via `Input.insertText` so Meet's React-controlled input picks it
//!     up as a real keystroke.
//!  3. Click "Ask to join" / "Join now".
//!
//! All three phases are best-effort. The runner logs and continues if a
//! phase times out so the caller can recover manually (e.g. an
//! interactive operator could finish the join in a non-headless build).

use std::time::Duration;

use serde_json::{json, Value};

use super::cdp::CdpConn;

/// Per-phase polling budgets. Tightened from the Shell-side scanner
/// since the headless runner does *not* have a fake-camera Y4M to
/// rasterize first — chromium boots in ~1.5 s and the device-check
/// screen either shows up immediately or never.
const DEVICE_CHECK_BUDGET: Duration = Duration::from_secs(8);
const NAME_INPUT_BUDGET: Duration = Duration::from_secs(30);
const JOIN_BUTTON_BUDGET: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Run the three-phase join. Returns Ok on success, Err with the phase
/// label baked in so the caller can log a useful message.
pub async fn run(cdp: &mut CdpConn, session: &str, display_name: &str) -> Result<(), String> {
    // Phase 1 — device check. Meet's exact copy varies by region;
    // we try the canonical English variants.
    if let Err(err) = wait_and_click_text(
        cdp,
        session,
        &[
            "Continue without microphone and camera",
            "Continue without microphone",
            "Continue without camera",
        ],
        DEVICE_CHECK_BUDGET,
    )
    .await
    {
        log::info!("[meet-headless] device-check dismissal not needed: {err}");
    }

    // Phase 2 — type the display name. Hard-fail if we can't find
    // the input: there is no useful way for the bot to proceed
    // without identifying itself.
    type_into_named_input(cdp, session, "Your name", display_name)
        .await
        .map_err(|e| format!("name input phase: {e}"))?;

    // Phase 3 — request to join.
    wait_and_click_text(
        cdp,
        session,
        &["Ask to join", "Join now"],
        JOIN_BUTTON_BUDGET,
    )
    .await
    .map_err(|e| format!("join button phase: {e}"))?;

    Ok(())
}

/// Repeatedly evaluate a click-by-text helper in the page until either
/// a click lands or `budget` elapses.
async fn wait_and_click_text(
    cdp: &mut CdpConn,
    session: &str,
    labels: &[&str],
    budget: Duration,
) -> Result<(), String> {
    let labels_js = serde_json::to_string(labels).map_err(|e| format!("labels json: {e}"))?;
    let expression = format!(
        r#"
        (() => {{
          const labels = {labels_js};
          const want = labels.map(l => l.toLowerCase());
          const candidates = document.querySelectorAll(
            'button, [role="button"], a[role="button"]'
          );
          for (const el of candidates) {{
            if (el.disabled || el.getAttribute('aria-disabled') === 'true') continue;
            const text = ((el.innerText || el.textContent) || '').trim().toLowerCase();
            if (!text) continue;
            if (!want.some(w => text.includes(w))) continue;
            const rect = el.getBoundingClientRect();
            if (rect.width === 0 || rect.height === 0) continue;
            el.scrollIntoView({{ block: 'center', inline: 'center' }});
            el.click();
            return text;
          }}
          return null;
        }})()
        "#
    );

    let deadline = tokio::time::Instant::now() + budget;
    let mut last_value = Value::Null;
    while tokio::time::Instant::now() < deadline {
        let res = cdp
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": false,
                }),
                Some(session),
            )
            .await?;
        let value = res
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null);
        if value.is_string() {
            log::info!(
                "[meet-headless] clicked {labels:?} text={}",
                value.as_str().unwrap_or("")
            );
            return Ok(());
        }
        last_value = value;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Err(format!(
        "timeout waiting for clickable element matching {labels:?} (last={last_value})"
    ))
}

/// Focus an `<input>` whose `aria-label` or `placeholder` contains
/// `hint`, then dispatch the supplied text via `Input.insertText`.
async fn type_into_named_input(
    cdp: &mut CdpConn,
    session: &str,
    hint: &str,
    text: &str,
) -> Result<(), String> {
    let hint_js = serde_json::to_string(hint).map_err(|e| format!("hint json: {e}"))?;
    let focus_expr = format!(
        r#"
        (() => {{
          const hint = {hint_js}.toLowerCase();
          const inputs = document.querySelectorAll('input');
          for (const inp of inputs) {{
            const t = (inp.getAttribute('type') || 'text').toLowerCase();
            if (t !== 'text' && t !== 'search') continue;
            const aria = (inp.getAttribute('aria-label') || '').toLowerCase();
            const ph = (inp.placeholder || '').toLowerCase();
            if (!aria.includes(hint) && !ph.includes(hint)) continue;
            inp.focus();
            inp.click();
            try {{ inp.select(); }} catch (_) {{}}
            return true;
          }}
          return false;
        }})()
        "#
    );

    let deadline = tokio::time::Instant::now() + NAME_INPUT_BUDGET;
    while tokio::time::Instant::now() < deadline {
        let res = cdp
            .call(
                "Runtime.evaluate",
                json!({ "expression": focus_expr, "returnByValue": true }),
                Some(session),
            )
            .await?;
        let focused = res
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if focused {
            cdp.call("Input.insertText", json!({ "text": text }), Some(session))
                .await?;
            log::info!(
                "[meet-headless] inserted display name (hint={hint} chars={})",
                text.chars().count()
            );
            return Ok(());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Err(format!("timeout waiting for input matching hint={hint}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_are_sane() {
        // Total join budget should stay under 90 s so a stuck join
        // never holds the headless session hostage past a reasonable
        // user-patience window.
        let total = DEVICE_CHECK_BUDGET + NAME_INPUT_BUDGET + JOIN_BUTTON_BUDGET;
        assert!(
            total <= Duration::from_secs(90),
            "total join budget {total:?} > 90s — likely accidental inflation"
        );
    }
}
