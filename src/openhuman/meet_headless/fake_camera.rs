use super::cdp::CdpConn;
use serde_json::json;

const LOG_PREFIX: &str = "[meet-headless-camera]";

/// JavaScript that overrides `navigator.mediaDevices.getUserMedia` to return
/// a fake video stream (canvas with "OpenHuman Agent" text) and silent audio
/// (0 Hz oscillator via AudioContext + MediaStreamDestination).
///
/// Injected via CDP `Page.addScriptToEvaluateOnNewDocument` so it runs before
/// any page JavaScript (i.e. before Google Meet loads).
const FAKE_CAMERA_OVERRIDE_JS: &str = r##"(function() {
    "use strict";

    // Guard: only run in a browser-like environment
    if (typeof navigator === "undefined" || !navigator.mediaDevices) {
        return;
    }

    // Save the original getUserMedia so we can fall back for unexpected calls
    var _origGetUserMedia = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);

    navigator.mediaDevices.getUserMedia = function(constraints) {
        var hasVideo = constraints && constraints.video;
        var hasAudio = constraints && constraints.audio;

        // If neither video nor audio is requested, fall back to the original
        if (!hasVideo && !hasAudio) {
            return _origGetUserMedia(constraints);
        }

        var streams = [];

        // --- Fake video: 640x480 canvas with dark background + label text ---
        if (hasVideo) {
            var canvas = document.createElement("canvas");
            canvas.width = 640;
            canvas.height = 480;
            var ctx = canvas.getContext("2d");

            // Dark background
            ctx.fillStyle = "#1a1a2e";
            ctx.fillRect(0, 0, canvas.width, canvas.height);

            // "OpenHuman Agent" text in coral
            ctx.fillStyle = "#e94560";
            ctx.font = "bold 28px sans-serif";
            ctx.textAlign = "center";
            ctx.textBaseline = "middle";
            ctx.fillText("OpenHuman Agent", canvas.width / 2, canvas.height / 2);

            // Capture at 15 fps
            var videoStream = canvas.captureStream(15);
            streams.push(videoStream);
        }

        // --- Fake audio: silent 0 Hz oscillator via AudioContext ---
        // Expose audioCtx + dest on window so the audio bridge can feed
        // its playback PCM into the same destination that Meet uses.
        if (hasAudio) {
            var audioCtx = new AudioContext({ sampleRate: 16000 });
            var oscillator = audioCtx.createOscillator();
            oscillator.frequency.value = 0;
            oscillator.type = "sine";

            var dest = audioCtx.createMediaStreamDestination();
            oscillator.connect(dest);
            oscillator.start();

            // Export for audio_bridge playback pipeline
            window.__openhuman_fake_audio = {
                ctx: audioCtx,
                dest: dest
            };

            var audioStream = dest.stream;
            streams.push(audioStream);
        }

        // Merge all tracks into a single MediaStream
        var combined = new MediaStream();
        streams.forEach(function(s) {
            s.getTracks().forEach(function(track) {
                combined.addTrack(track);
            });
        });

        return Promise.resolve(combined);
    };
})();
"##;

/// Inject the fake-camera override JavaScript via CDP.
///
/// Uses `Page.addScriptToEvaluateOnNewDocument` so the override is applied
/// before any page JavaScript executes. When `session_id` is `None` the
/// call targets the browser-level session (suitable for injecting before
/// `Target.createTarget`).
pub async fn inject_fake_camera(
    cdp: &mut CdpConn,
    session_id: Option<&str>,
) -> Result<(), String> {
    log::info!(
        "{} injecting fake-camera override via Page.addScriptToEvaluateOnNewDocument",
        LOG_PREFIX
    );

    let params = json!({
        "source": FAKE_CAMERA_OVERRIDE_JS,
        "worldName": "",
        "includeCommandLineAPI": false,
        "runImmediately": true,
    });

    cdp.call(
        "Page.addScriptToEvaluateOnNewDocument",
        params,
        session_id,
    )
    .await
    .map_err(|e| {
        format!(
            "{} failed to inject fake-camera override: {}",
            LOG_PREFIX, e
        )
    })?;

    log::info!("{} fake-camera override injected successfully", LOG_PREFIX);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_contains_get_user_media() {
        assert!(
            FAKE_CAMERA_OVERRIDE_JS.contains("getUserMedia"),
            "JS must override getUserMedia"
        );
    }

    #[test]
    fn js_contains_capture_stream() {
        assert!(
            FAKE_CAMERA_OVERRIDE_JS.contains("captureStream"),
            "JS must use captureStream for fake video"
        );
    }

    #[test]
    fn js_contains_audio_context() {
        assert!(
            FAKE_CAMERA_OVERRIDE_JS.contains("AudioContext"),
            "JS must create AudioContext for fake audio"
        );
    }

    #[test]
    fn js_contains_oscillator() {
        assert!(
            FAKE_CAMERA_OVERRIDE_JS.contains("oscillator"),
            "JS must create oscillator for silent audio"
        );
    }

    #[test]
    fn js_balanced_braces() {
        let open = FAKE_CAMERA_OVERRIDE_JS.chars().filter(|c| *c == '{').count();
        let close = FAKE_CAMERA_OVERRIDE_JS.chars().filter(|c| *c == '}').count();
        assert_eq!(
            open, close,
            "JS must have balanced curly braces (found {} open, {} close)",
            open, close
        );
    }

    #[test]
    fn js_balanced_parentheses() {
        let open = FAKE_CAMERA_OVERRIDE_JS.chars().filter(|c| *c == '(').count();
        let close = FAKE_CAMERA_OVERRIDE_JS.chars().filter(|c| *c == ')').count();
        assert_eq!(
            open, close,
            "JS must have balanced parentheses (found {} open, {} close)",
            open, close
        );
    }

    #[test]
    fn js_uses_iife_pattern() {
        assert!(
            FAKE_CAMERA_OVERRIDE_JS.starts_with("(function()"),
            "JS must use IIFE pattern"
        );
    }

    #[test]
    fn js_contains_canvas_dimensions() {
        assert!(
            FAKE_CAMERA_OVERRIDE_JS.contains("640") && FAKE_CAMERA_OVERRIDE_JS.contains("480"),
            "JS must set canvas to 640x480"
        );
    }

    #[test]
    fn js_contains_dark_background_color() {
        assert!(
            FAKE_CAMERA_OVERRIDE_JS.contains("#1a1a2e"),
            "JS must use dark background color #1a1a2e"
        );
    }

    #[test]
    fn js_contains_coral_text_color() {
        assert!(
            FAKE_CAMERA_OVERRIDE_JS.contains("#e94560"),
            "JS must use coral text color #e94560"
        );
    }

    #[test]
    fn js_contains_openhuman_agent_text() {
        assert!(
            FAKE_CAMERA_OVERRIDE_JS.contains("OpenHuman Agent"),
            "JS must draw 'OpenHuman Agent' text on canvas"
        );
    }

    #[test]
    fn js_saves_original_get_user_media() {
        assert!(
            FAKE_CAMERA_OVERRIDE_JS.contains("_origGetUserMedia"),
            "JS must save the original getUserMedia for fallback"
        );
    }

    #[test]
    fn log_prefix_is_correct() {
        assert_eq!(LOG_PREFIX, "[meet-headless-camera]");
    }
}
