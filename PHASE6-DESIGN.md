# Phase 6: ヘッドレス対応 — Playwright ベース Meet 参加

## 概要

現在の Meet Agent は Tauri + CEF（Chromium Embedded Framework）で
Google Meet に参加している。Shell（デスクトップアプリ）が CEF webview
を開き、CDP 経由で自動操作し、CEF の `AudioHandler` で音声をキャプチャ
している。

Phase 6 では、CEF に依存せず Playwright（Headless Chromium）で
Meet に参加する仕組みを実装する。これにより:
- **サーバー/CLI での動作**が可能（デスクトップアプリ不要）
- **OpenClaw Skill からの直接制御**が可能
- **CI/CD でのテスト**が可能

## 現在のアーキテクチャ（CEF 依存）

```
┌─────────────────────────────────────────────┐
│ Tauri Shell (app/src-tauri/)                │
│                                             │
│ meet_call ──→ CEF Webview Window            │
│    │           (meet.google.com)             │
│    │                                         │
│ meet_scanner ──→ CDP ──→ 自動操作           │
│    (名前入力, 「参加」クリック)              │
│                                             │
│ meet_audio/listen_capture                    │
│    CEF AudioHandler ──→ PCM16LE ──→ Core    │
│                                             │
│ meet_audio/speak_pump                        │
│    Core PCM ──→ Chromium pipe:// ──→ Meet   │
│                                             │
│ meet_audio/caption_listener                  │
│    recipe.js ──→ DOM監視 ──→ Core           │
│                                             │
│ meet_video/inject                            │
│    Fake Camera (Y4M) ──→ Meet              │
└─────────────────────────────────────────────┘
          │ RPC (JSON-RPC :7788)
          ▼
┌─────────────────────────────────────────────┐
│ OpenHuman Core (src/openhuman/)             │
│ meet_agent/brain.rs (STT→LLM→TTS)          │
│ meet_agent/session.rs (セッション状態)       │
│ meet_agent/providers/ (プロバイダー)        │
└─────────────────────────────────────────────┘
```

## 目標アーキテクチャ（Playwright）

```
┌─────────────────────────────────────────────┐
│ Headless Runner (新規 Rust モジュール)       │
│ src/openhuman/meet_headless/                │
│                                             │
│ runner.rs ──→ Playwright (chromium)         │
│    │           (headless Meet 参加フロー)    │
│    │                                         │
│ join_flow.rs                                │
│    Playwright自動操作 (名前入力, 参加)       │
│                                             │
│ audio_bridge.rs                             │
│    Page音声 → PCM16LE → Core RPC            │
│    Core PCM → Page AudioContext             │
│                                             │
│ caption_watcher.rs                          │
│    JS評価 → DOM監視 → Core RPC              │
│                                             │
│ fake_camera.rs                              │
│    MediaStream API でダミー映像             │
└─────────────────────────────────────────────┘
          │ RPC (JSON-RPC :7788)
          ▼
┌─────────────────────────────────────────────┐
│ OpenHuman Core (src/openhuman/)             │
│ meet_agent/brain.rs (STT→LLM→TTS)          │
│ meet_agent/session.rs (セッション状態)       │
│ meet_agent/providers/ (プロバイダー)        │
└─────────────────────────────────────────────┘
```

## サブフェーズ

### 6.1: Playwright 基盤（Runner + Join Flow）
**規模**: 中
**担当**: Claude Code

- `src/openhuman/meet_headless/mod.rs` — モジュール定義
- `src/openhuman/meet_headless/runner.rs` — Playwright セッション管理
  - Playwright ブラウザ起動（headless chromium）
  - Meet URL へのナビゲーション
  - セッションライフサイクル（start/stop）
- `src/openhuman/meet_headless/join_flow.rs` — 自動参加フロー
  - `meet_scanner/mod.rs` の CDP ロジックを Playwright に移植
  - デバイスチェック解除 → 名前入力 → 「参加」クリック
  - タイムアウト + エラーハンドリング

**依存**: `chromiumoxide` crate（Rust Playwright クライアント）
  - または `playwright` CLI via 子プロセス
  - 推奨: `chromiumoxide`（純 Rust、CDP 直結）

### 6.2: キャプション監視（Caption Watcher）
**規模**: 小
**担当**: Claude Code

- `src/openhuman/meet_headless/caption_watcher.rs`
- `recipes/google-meet/recipe.js` のキャプション監視ロジックを移植
- Playwright の `page.evaluate()` で DOM 監視スクリプトを注入
- キャプション変更時に `openhuman.meet_agent_push_caption` RPC を呼び出し
- Meet の `[jsname="tgaKEf"]` セレクタ対応

### 6.3: 音声ブリッジ（Audio Bridge）
**規模**: 大
**担当**: Claude Code

- `src/openhuman/meet_headless/audio_bridge.rs`
- **受信（Meet → Core）**: 
  - Chrome Audio Capture API または `--audio-output` フラグ
  - 代替案: Web Audio API + `MediaRecorder` → Base64 → WebSocket → Rust
- **送信（Core → Meet）**: 
  - `AudioContext` + `MediaStreamSource` でダミー音声デバイスを作成
  - Core から `poll_speech` PCM を取得 → JS に渡す → 再生
- PCM16LE 16kHz へのリサンプリング

### 6.4: フェイクカメラ（Fake Camera）
**規模**: 小
**担当**: Claude Code

- `src/openhuman/meet_headless/fake_camera.rs`
- 現在の CEF では Y4M ファイルを fake camera として使用
- Playwright では `page.addInitScript()` で `MediaStream` API を override
- 黒画面 or ロゴ画像をダミー映像として提供

### 6.5: RPC 統合 + テスト
**規模**: 中
**担当**: Claude Code → Codex レビュー

- `meet_headless` の RPC エンドポイント追加
  - `openhuman.meet_headless_start` — Playwright セッション開始
  - `openhuman.meet_headless_stop` — セッション終了
- 統合テスト（wiremock で Meet DOM をモック）
- OpenClaw Skill (`~/.agents/skills/openhuman-meet/`) の更新
  - `meet-join` に `--headless` オプション追加

### 6.6: ドキュメント + PR
**規模**: 小
**担当**: Jarvis

- DESIGN.md Phase 6 セクション追加
- IMPL-PLAN.md 更新
- MEMORY.md 更新
- PR 作成 → Codex レビュー → マージ

## 技術的決定事項

### Playwright クライアント選択

| オプション | メリット | デメリット |
|-----------|---------|-----------|
| `chromiumoxide` (Rust crate) | 純 Rust、CDP直結、依存少 | 低レベル、メンテ不安定性 |
| Playwright CLI (subprocess) | 安定、公式サポート | 子プロセス管理の複雑さ |
| `fantoccini` (WebDriver) | 汎用的 | Meet の自動操作に不向き |

**推奨**: `chromiumoxide` — Rustエコシステム内で完結、CDP操作が `meet_scanner` と同じパターン

### 音声キャプチャ方式

| 方式 | メリット | デメリット |
|------|---------|-----------|
| Chrome `--audio-output` flag | シンプル | ファイルベース、遅延大 |
| Web Audio API + WebSocket | リアルタイム | JS注入が複雑 |
| CDP `Audio.serializeNodes` | 低レベルアクセス | ドキュメント不足 |
| Page MediaRecorder → base64 | 実装容易 | エンコードオーバーヘッド |

**推奨**: Web Audio API + WebSocket — リアルタイム性と実装バランス

## リスク

| リスク | 対策 |
|--------|------|
| Meet DOM構造の変更 | セレクタを設定ファイル化 |
| Headless検知でMeetがブロック | User-Agent設定 + stealth plugin |
| 音声遅延が大きい | Web Audio API でバッファ最適化 |
| chromiumoxide の不安定性 | フォールバックで Playwright CLI |

## 実行順序

```
6.1 (Runner + Join) → 6.2 (Caption) → 6.3 (Audio) → 6.4 (Camera) → 6.5 (RPC + Test) → 6.6 (Doc + PR)
```

6.1-6.2 は独立して実装可能。6.3 が最も複雑。
