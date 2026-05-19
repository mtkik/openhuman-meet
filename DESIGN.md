# OpenHuman Meet Agent — Self-Hosted Fork 設計書

> **プロジェクト**: mtkik/openhuman-meet  
> **ベース**: tinyhumansai/openhuman v0.54.0  
> **ライセンス**: GNU GPL-3.0（フォーク継承）  
> **目的**: OpenHumanのMeet Agent機能を自前API（無料/安価プロバイダー）で動くようにし、OpenClaw Skillからも利用可能にする

---

## 1. 背景と課題

### 1.1 現状の問題

OpenHuman v0.54.0のMeet Agentは、STT/LLM/TTSすべてが `api.tinyhumans.ai` バックエンド経由で、session token（要サブスクリプション）が必須。

```
brain.rs::stt()  → cloud_transcribe → api.tinyhumans.ai/openai/v1/audio/transcriptions
brain.rs::llm()  → BackendOAuthClient → api.tinyhumans.ai/openai/v1/chat/completions
brain.rs::tts()  → reply_speech → api.tinyhumans.ai/openai/v1/audio/speech (ElevenLabs)
```

session tokenなし → 即エラー、フォールバックなし。

### 1.2 目標

1. **自前API差し替え**: STT/LLM/TTSを設定可能なプロバイダーに抽象化
2. **OpenClaw Skill化**: JSON-RPC経由でOpenClawからMeet Agentを操作可能に
3. **ローカル完結**: 可能な限りローカルモデル（Ollama等）で動くように
4. **GUI不要**: ヘッドレス/CLIでMeet参加可能に

---

## 2. 影響範囲分析

### 2.1 変更が必要なファイル（brain.rsの3関数）

| ファイル | 行数 | 変更内容 |
|---------|------|---------|
| `src/openhuman/meet_agent/brain.rs` | ~780 | `stt()`, `llm_meeting()`, `tts()` の3関数をトレイトに抽出 |
| `src/openhuman/inference/voice/cloud_transcribe.rs` | ~150 | プロバイダー切り替え可能に |
| `src/openhuman/voice/reply_speech.rs` | ~180 | プロバイダー切り替え可能に |
| `src/openhuman/meet_agent/mod.rs` | ~60 | トレイト登録・DI |
| `src/openhuman/config/schema/meet.rs` | 新規 | Meet Agent用設定スキーマ |
| `src/openhuman/config/schemas.rs` | ~100 | meet.rsのスキーマ登録 |

### 2.2 変更不要なファイル（そのまま使える）

| ファイル | 役割 | 理由 |
|---------|------|------|
| `meet_call/mod.rs` | ウィンドウ管理 | CEF機能、変更不要 |
| `meet_scanner/mod.rs` | CDP自動参加 | CDP操作、変更不要 |
| `meet_audio/*` | 音声パイプライン | Shell側、変更不要 |
| `meet_video/*` | カメラフレーム | Shell側、変更不要 |
| `cdp/*` | CDP接続管理 | Shell側、変更不要 |
| `recipes/google-meet/recipe.js` | キャプション監視 | ブラウザ内JS、変更不要 |
| `meet_agent/session.rs` | VAD・セッション管理 | Core内ロジック、変更不要 |
| `meet_agent/ops.rs` | VAD・リングバッファ | 純粋ロジック、変更不要 |
| `meet_agent/types.rs` | 型定義 | 変更不要 |
| `meet_agent/wav.rs` | PCM→WAV変換 | 変更不要 |
| `meet_agent/rpc.rs` | JSON-RPCハンドラ | 変更不要 |

---

## 3. アーキテクチャ設計

### 3.1 Provider Trait抽象化

**※アーキテクチャの重要決定事項 — ko1さんと相談して決める**

#### 案A: 3つの独立トレイト

```rust
/// STT プロバイダー
#[async_trait]
trait SpeechToText: Send + Sync {
    async fn transcribe(&self, pcm: &[i16], sample_rate: u32) -> Result<String, String>;
}

/// LLM プロバイダー
#[async_trait]
trait MeetingLLM: Send + Sync {
    async fn reply(&self, prompt: &str, history: &[ConversationTurn]) -> Result<String, String>;
}

/// TTS プロバイダー
#[async_trait]
trait TextToSpeech: Send + Sync {
    async fn synthesize(&self, text: &str) -> Result<Vec<i16>, String>;
}
```

#### 案B: 単一のAgentPipelineトレイト

```rust
#[async_trait]
trait MeetAgentPipeline: Send + Sync {
    async fn transcribe(&self, pcm: &[i16], sample_rate: u32) -> Result<String, String>;
    async fn reply(&self, prompt: &str, history: &[ConversationTurn]) -> Result<String, String>;
    async fn synthesize(&self, text: &str) -> Result<Vec<i16>, String>;
}
```

### 3.2 設定スキーマ（案）

```toml
# ~/.openhuman/config.toml

[meet_agent]
# パイプラインプロバイダー選択
stt_provider = "google"      # "google" | "openai" | "local_whisper" | "tinyhumans"
llm_provider = "gemini"      # "gemini" | "openai" | "ollama" | "tinyhumans"
tts_provider = "google"      # "google" | "openai" | "elevenlabs" | "tinyhumans"

[meet_agent.providers.google]
api_key = "..."              # または環境変数 GOOGLE_API_KEY
stt_model = "chirp_2"
tts_voice = "ja-JP-Standard-A"

[meet_agent.providers.gemini]
api_key = "..."              # または環境変数 GEMINI_API_KEY
model = "gemini-2.5-flash"

[meet_agent.providers.ollama]
base_url = "http://localhost:11434"
model = "gemma3:4b"

[meet_agent.providers.openai]
api_key = "..."
model = "gpt-4o-mini"
tts_model = "tts-1"

[meet_agent.providers.elevenlabs]
api_key = "..."
voice_id = "..."
model_id = "eleven_turbo_v2_5"

[meet_agent.providers.tinyhumans]
# 既存のtinyhumansバックエンド（そのまま）
# session_tokenは既存の認証フローを使用
```

### 3.3 プロバイダー実装

#### STT プロバイダー候補

| プロバイダー | 無料枠 | レイテンシ | 品質 | 備考 |
|-------------|--------|----------|------|------|
| Google Speech-to-Text | 月60分 | 低 | 高 | v2/chirp推奨 |
| OpenAI Whisper API | なし（$0.006/分） | 中 | 高 | |
| local Whisper (whisper.cpp) | 無料 | GPU依存 | 中 | 要Ollama/ローカル |
| tinyhumans（既存） | 要課金 | 低 | 高 | バックエンドプロキシ |

#### LLM プロバイダー候補

| プロバイダー | 無料枠 | レイテンシ | 備考 |
|-------------|--------|----------|------|
| Gemini Flash | 15RPM/1500RPD | 低 | 無料枠で十分な会話量 |
| OpenAI gpt-4o-mini | なし | 低 | |
| Ollama (ローカル) | 無料 | 中〜高 | M1 Mac miniで実用レベル |
| tinyhumans（既存） | 要課金 | 低 | |

#### TTS プロバイダー候補

| プロバイダー | 無料枠 | 品質 | 備考 |
|-------------|--------|------|------|
| Google Cloud TTS | 月100万文字 | 高 | ja-JP対応 |
| OpenAI TTS | なし | 高 | |
| ElevenLabs | 月1万文字 | 最高 | 既存実装の流用 |
| edge-tts (無料) | 無料 | 中 | Microsoft Edge読み上げ |
| Piper (ローカル) | 無料 | 中 | ONNX推論 |

---

## 4. OpenClaw Skill インターフェース

### 4.1 構成

```
~/.agents/skills/openhuman-meet/
├── SKILL.md
├── scripts/
│   ├── meet-join          # Meet参加スクリプト
│   ├── meet-status        # ステータス確認
│   └── meet-leave         # Meet退出
├── data/
│   └── default-config.toml  # デフォルト設定
└── tests/
    └── test_unit.py
```

### 4.2 利用フロー

```
Jarvis → OpenClaw Skill → JSON-RPC → OpenHuman Core (:7788)
                                    → OpenHuman Shell (CEF)
                                    → Google Meet
```

### 4.3 JSON-RPC呼び出し例

```bash
# Meet参加
curl -X POST http://127.0.0.1:7788/rpc \
  -H "Authorization: Bearer ${TOKEN}" \
  -d '{"jsonrpc":"2.0","method":"openhuman.meet_join_call","params":{"meet_url":"https://meet.google.com/abc-defg-hij","display_name":"Jarvis"},"id":1}'

# キャプション取得
curl -X POST http://127.0.0.1:7788/rpc \
  -H "Authorization: Bearer ${TOKEN}" \
  -d '{"jsonrpc":"2.0","method":"openhuman.meet_agent_push_caption","params":{"request_id":"...","speaker":"Alice","text":"Hello everyone","ts_ms":12345},"id":2}'

# Meet退出
curl -X POST http://127.0.0.1:7788/rpc \
  -H "Authorization: Bearer ${TOKEN}" \
  -d '{"jsonrpc":"2.0","method":"openhuman.meet_call_close_window","params":{"request_id":"..."},"id":3}'
```

---

## 5. 実装フェーズ

### Phase 1: Provider Trait抽出（最重要）
- `brain.rs` の3関数をトレイトに抽出
- 設定スキーマ追加
- デフォルトプロバイダー = 既存tinyhumans（動作互換維持）
- **目標**: 既存テストが全て通ること

### Phase 2: 自前プロバイダー実装
- Google STT プロバイダー
- Gemini LLM プロバイダー
- Google TTS プロバイダー
- 設定で切り替え可能に

### Phase 3: OpenClaw Skill
- SKILL.md作成
- Helper CLI作成
- JSON-RPC連携

### Phase 4: ヘッドレス対応（別途検討）
- Shell（CEF）なしでCore + PlaywrightでMeet参加
- brain.rsの変更だけでなくShell側の変更も必要

---

## 6. 相談事項 — 決定済み（2026-05-20）

1. **Provider Trait設計** → **案A: 3つの独立トレイト**（STT/LLM/TTS別々に切り替え可能）
2. **デフォルトプロバイダー** → Google STT（月60分無料）+ Gemini Flash（15RPM無料）+ Google TTS（月100万文字無料）
3. **ヘッドレス対応** → Phase 1から組み込む
4. **設定ファイル形式** → TOML（OpenHuman既存形式と統一）
5. **フォーク公開/非公開** → まずはprivate

---

## 7. リスクと注意点

| リスク | 対策 |
|--------|------|
| OpenHuman本体のアップデート追従 | upstreamをremoteに追加、rebaseで追従 |
| Google API無料枠の制限 | 設定でプロバイダー切り替え可能に |
| Meet DOM構造変更 | recipe.jsはOpenHuman側がメンテ、追随が必要 |
| ビルド環境（Rust 1.93固定） | rust-toolchain.tomlで固定済み |
| CEF依存のShellはフォークだけでは解決しない | Phase 4で別途対応 |

---

_この設計書はko1さんとの相談を経て確定します。_
_作成日: 2026-05-20_
