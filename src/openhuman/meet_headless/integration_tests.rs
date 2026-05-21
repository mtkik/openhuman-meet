//! Phase 6.5 — Integration tests for meet_headless module.
//!
//! These tests verify cross-module wiring that unit tests can't catch:
//! - Schema registration completeness
//! - RPC handler parameter validation
//! - Audio bridge PCM pipeline integrity
//! - Caption parsing edge cases
//! - Fake camera JS structural validity
//! - Module registration in the crate

#[cfg(test)]
mod tests {
    use crate::openhuman::meet_headless::schemas::{
        all_controller_schemas, all_registered_controllers,
    };
    use crate::openhuman::meet_headless::audio_bridge::{
        pcm_to_base64, base64_to_pcm, is_silence,
    };
    use crate::openhuman::meet_headless::caption::parse_caption_rows;
    use crate::openhuman::meet_headless::runner::{redact_ws_url, parse_devtools_line};
    use serde_json::{json, Value};

    // ── Schema + Controller Wiring ──────────────────────────────

    #[test]
    fn all_schemas_have_matching_handlers() {
        let schemas = all_controller_schemas();
        let controllers = all_registered_controllers();
        assert_eq!(schemas.len(), controllers.len(),
            "schema count ({}) != handler count ({})", schemas.len(), controllers.len());

        for (i, (s, c)) in schemas.iter().zip(controllers.iter()).enumerate() {
            assert_eq!(s.function, c.schema.function,
                "schema[{}].function ({}) != controller[{}].schema.function ({})",
                i, s.function, i, c.schema.function);
            assert_eq!(s.namespace, c.schema.namespace,
                "schema[{}].namespace ({}) != controller[{}].schema.namespace ({})",
                i, s.namespace, i, c.schema.namespace);
        }
    }

    #[test]
    fn rpc_surface_covers_start_and_stop() {
        let schemas = all_controller_schemas();
        let functions: Vec<String> = schemas.iter().map(|s| s.function.to_string()).collect();
        assert!(functions.contains(&"start".to_string()), "missing 'start' RPC");
        assert!(functions.contains(&"stop".to_string()), "missing 'stop' RPC");
        assert_eq!(functions.len(), 2, "expected exactly 2 RPC endpoints, got {}", functions.len());
    }

    #[test]
    fn namespace_is_meet_headless() {
        for s in all_controller_schemas() {
            assert_eq!(s.namespace, "meet_headless",
                "expected namespace 'meet_headless', got '{}'", s.namespace);
        }
    }

    // ── Audio Bridge Pipeline ──────────────────────────────────

    #[test]
    fn pcm_pipeline_large_buffer_roundtrip() {
        // 10 seconds of audio at 16kHz = 160,000 samples
        let samples: Vec<i16> = (0..160_000)
            .map(|i| ((i as f64 * 0.1).sin() * 16000.0) as i16)
            .collect();
        let b64 = pcm_to_base64(&samples);
        let decoded = base64_to_pcm(&b64).unwrap();
        assert_eq!(samples, decoded, "large buffer roundtrip failed");
    }

    #[test]
    fn pcm_pipeline_all_extremes() {
        let extremes: Vec<i16> = vec![i16::MIN, i16::MAX, 0, -1, 1];
        let b64 = pcm_to_base64(&extremes);
        let decoded = base64_to_pcm(&b64).unwrap();
        assert_eq!(extremes, decoded, "extreme values roundtrip failed");
    }

    #[test]
    fn silence_detection_threshold_boundary() {
        let samples: Vec<i16> = vec![99, -99, 0, 50, -50];
        assert!(is_silence(&samples, 100), "should be silent at threshold 100");
        assert!(!is_silence(&samples, 98), "should NOT be silent at threshold 98");
    }

    #[test]
    fn silence_detection_single_loud_sample() {
        let samples: Vec<i16> = vec![0, 0, 0, 500, 0, 0, 0];
        assert!(is_silence(&samples, 500));
        assert!(!is_silence(&samples, 499));
    }

    // ── Caption Parsing Integration ────────────────────────────

    #[test]
    fn caption_parsing_real_world_structure() {
        // Simulate what DRAIN_SCRIPT returns from a real Meet page
        let v = json!([
            {"speaker": "Alice", "text": "Hey everyone, let's get started"},
            {"speaker": "", "text": "Sure, I'll share my screen"},
            {"speaker": "Bob", "text": "Can you see my screen?"},
            {"speaker": "Alice", "text": "Yes, looks good"},
            {"speaker": "Charlie", "text": "Sorry I'm late"}
        ]);
        let rows = parse_caption_rows(&v);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].speaker, "Alice");
        assert_eq!(rows[1].speaker, ""); // self participant, no img alt
        assert_eq!(rows[2].text, "Can you see my screen?");
    }

    #[test]
    fn caption_parsing_mixed_valid_and_invalid() {
        let v = json!([
            {"speaker": "Alice", "text": "Valid caption"},
            {"speaker": "Bob", "text": ""},
            {"text": "No speaker but valid text"},
            {"speaker": "", "text": "   "},
            "not an object",
            {"speaker": "Charlie", "text": "Also valid"}
        ]);
        let rows = parse_caption_rows(&v);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].text, "Valid caption");
        assert_eq!(rows[1].text, "No speaker but valid text");
        assert_eq!(rows[2].text, "Also valid");
    }

    #[test]
    fn caption_parsing_empty_and_null() {
        assert!(parse_caption_rows(&Value::Null).is_empty());
        assert!(parse_caption_rows(&json!([])).is_empty());
        assert!(parse_caption_rows(&json!("string")).is_empty());
        assert!(parse_caption_rows(&json!(42)).is_empty());
    }

    // ── Runner Utility Integration ─────────────────────────────

    #[test]
    fn redact_ws_url_various_formats() {
        // Standard format
        assert_eq!(
            redact_ws_url("ws://127.0.0.1:9222/devtools/browser/abc-def-123"),
            "ws://127.0.0.1:9222/devtools/browser/*****"
        );
        // Non-standard input
        assert_eq!(redact_ws_url("not-a-url"), "*****");
        // Minimal ws URL
        assert_eq!(redact_ws_url("ws://"), "ws://*****");
    }

    #[test]
    fn parse_devtools_various_banners() {
        // Standard banner
        assert!(parse_devtools_line("DevTools listening on ws://127.0.0.1:9222/devtools/browser/uuid")
            .is_some());
        // Not the banner
        assert!(parse_devtools_line("Some other log line").is_none());
        // Wrong scheme
        assert!(parse_devtools_line("DevTools listening on http://example.com").is_none());
        // Empty
        assert!(parse_devtools_line("").is_none());
    }

    // ── Fake Camera JS Integration ─────────────────────────────

    #[test]
    fn fake_camera_js_exports_audio_for_bridge() {
        // After P1 fix, fake camera must export __openhuman_fake_audio
        use crate::openhuman::meet_headless::fake_camera::FAKE_CAMERA_OVERRIDE_JS;
        assert!(
            FAKE_CAMERA_OVERRIDE_JS.contains("window.__openhuman_fake_audio"),
            "fake camera JS must export __openhuman_fake_audio for audio bridge"
        );
        assert!(
            FAKE_CAMERA_OVERRIDE_JS.contains("audioCtx"),
            "__openhuman_fake_audio must expose audioCtx"
        );
        assert!(
            FAKE_CAMERA_OVERRIDE_JS.contains("dest"),
            "__openhuman_fake_audio must expose dest"
        );
    }

    // ── Cross-Module: Audio scripts reference fake camera ──────

    #[test]
    fn playback_js_reads_fake_camera_exports() {
        // After P1 fix, playback must check window.__openhuman_fake_audio
        use crate::openhuman::meet_headless::audio_bridge::AUDIO_PLAYBACK_SETUP_JS;
        assert!(
            AUDIO_PLAYBACK_SETUP_JS.contains("window.__openhuman_fake_audio"),
            "playback JS must check for fake camera exports"
        );
    }

    #[test]
    fn capture_js_uses_media_elements_not_mic() {
        // After P1 fix, capture must NOT use getUserMedia
        use crate::openhuman::meet_headless::audio_bridge::AUDIO_CAPTURE_SETUP_JS;
        assert!(
            !AUDIO_CAPTURE_SETUP_JS.contains("getUserMedia"),
            "capture JS must NOT use getUserMedia (should use MediaElementAudioSourceNode)"
        );
        assert!(
            AUDIO_CAPTURE_SETUP_JS.contains("MediaElementAudioSource"),
            "capture JS must use MediaElementAudioSourceNode"
        );
        assert!(
            AUDIO_CAPTURE_SETUP_JS.contains("querySelectorAll('audio, video'"),
            "capture JS must scan for audio/video elements"
        );
    }

    #[test]
    fn playback_js_has_scheduling() {
        // After P1 fix, playback must use nextStartTime
        use crate::openhuman::meet_headless::audio_bridge::AUDIO_PLAYBACK_SETUP_JS;
        assert!(
            AUDIO_PLAYBACK_SETUP_JS.contains("nextStartTime"),
            "playback JS must use nextStartTime for jitter-free scheduling"
        );
    }
}
