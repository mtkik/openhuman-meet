# OpenHuman Meet Agent — 全体実装計画

## 進捗

### ✅ Phase 1: Provider Trait抽出（完了）
- [x] 3トレイト定義: `SpeechToText`, `MeetingLLM`, `TextToSpeech`
- [x] noop プロバイダー
- [x] tinyhumans プロバイダー（既存コードのラップ）
- [x] factory.rs（プロバイダー生成）
- [x] MeetAgentConfig（TOML設定スキーマ）
- [x] brain.rs リファクタ（trait呼び出し化）

### ✅ Phase 2: 自前プロバイダー実装（完了）
- [x] google_stt.rs — Google Cloud Speech-to-Text v1 REST API
- [x] gemini_llm.rs — Gemini Flash (OpenAI互換エンドポイント)
- [x] google_tts.rs — Google Cloud Text-to-Speech v1 REST API
- [x] factory.rs 更新 — "google"/"gemini" マッピング追加（init失敗時tinyhumansフォールバック付き）
- [x] mod.rs 更新 — モジュール登録
- [x] cargo check 通過確認

### 🔲 Phase 3: ビルド検証
- [ ] `cargo check` 通過確認
- [ ] 既存テスト通過確認
- [ ] 新規プロバイダーのユニットテスト

### 🔲 Phase 4: OpenClaw Skill化
- [ ] SKILL.md作成
- [ ] Helper CLI作成
- [ ] JSON-RPC連携スクリプト

### 🔲 Phase 5: レビュー
- [ ] Codexによる全コードレビュー

---

## 分担

### Hermes: Phase 2（自前プロバイダー実装）
- Google STT / Gemini LLM / Google TTS の3ファイル
- factory.rs の更新
- cargo check でコンパイル確認

### Claude Code: Phase 4（OpenClaw Skill化）
- Phase 2完了後に着手
- SKILL.md + Helper CLI + JSON-RPC連携

### Codex: Phase 5（レビュー）
- Phase 2+3完了後に全コードレビュー

---

## 情報共有（両エージェントに共有）

### リポジトリ
- `/tmp/openhuman-meet` (branch: `feat/provider-traits`)
- Remote: `origin` = `mtkik/openhuman-meet`, `upstream` = `tinyhumansai/openhuman`

### 既存構造
```
src/openhuman/meet_agent/
├── brain.rs           ← リファクタ済み（trait呼び出し）
├── mod.rs             ← providers モジュール登録済み
├── ops.rs             ← 変更不要
├── rpc.rs             ← 変更不要
├── session.rs         ← 変更不要
├── types.rs           ← 変更不要
├── wav.rs             ← 変更不要
└── providers/
    ├── mod.rs          ← トレイト定義
    ├── factory.rs      ← プロバイダー生成
    ├── noop.rs         ← テスト用
    └── tinyhumans.rs   ← 既存バックエンドラップ

src/openhuman/config/schema/
├── meet.rs            ← MeetAgentConfig追加済み
└── mod.rs             ← re-export追加済み
```

### トレイト仕様
```rust
// providers/mod.rs
trait SpeechToText: Send + Sync {
    async fn transcribe(&self, pcm: &[i16], sample_rate: u32) -> Result<String, String>;
}

trait MeetingLLM: Send + Sync {
    async fn reply(&self, prompt: &str, history: &[ConversationTurn], 
                   system_prompt: &str, max_tokens: u32) -> Result<String, String>;
}

trait TextToSpeech: Send + Sync {
    async fn synthesize(&self, text: &str, sample_rate: u32) -> Result<Vec<i16>, String>;
}
```

### 設定TOML
```toml
[meet.meet_agent]
stt_provider = "google"
llm_provider = "gemini"
tts_provider = "google"

[meet.meet_agent.providers.google]
api_key = "AIza..."  # or env GOOGLE_API_KEY
stt_model = "chirp_2"
tts_voice = "ja-JP-Standard-A"

[meet.meet_agent.providers.gemini]
api_key = "AIza..."  # or env GEMINI_API_KEY
model = "gemini-2.5-flash"
```

### 依存関係（既にCargo.tomlにある）
- `async-trait = "0.1"`
- `reqwest` (with JSON/multipart)
- `serde` / `serde_json`
- `base64`
- `tokio`

### Rust制約
- Edition 2021
- Toolchain 1.93.0 (rust-toolchain.toml)
