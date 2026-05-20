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

### ✅ Phase 3: ビルド検証（完了）
- [x] `cargo check --lib` 通過（既存warningのみ、新規エラーなし）
- [x] `cargo test -p openhuman --lib providers` — 902テスト通過
- [x] 新規プロバイダーのユニットテスト38個追加（wiremock）

### ✅ Phase 4: OpenClaw Skill化（完了）
- [x] SKILL.md作成（9KB、YAML frontmatter付き）
- [x] Helper CLI 4本: meet-join, meet-status, meet-leave, meet-transcript
- [x] JSON-RPC連携スクリプト
- [x] data/default-config.toml（デフォルト設定）

### ✅ Phase 5: レビュー＆修正（完了）
- [x] Claude Code レビュー — 8件指摘（P0×2, P1×5, P2×2）→全修正
- [x] Codex (gpt-5.5) レビュー — 3件指摘（P1×1, P2×2）→全修正
- [x] Codex PR レビュー — P2×2件（drain順序）
- [x] PR #1 作成 → squash merge 済み (a365e70)

### ✅ レビュー指摘修正（完了）
- [x] P0: brain.rs に config→factory 配線（load_providers helper）
- [x] P0: APIキー URL→X-goog-api-key ヘッダー変更
- [x] P1: STT デフォルトモデル chirp_2→latest_long
- [x] P1: HTTP 20s タイムアウト追加
- [x] P1: TTS WAV ヘッダースキップ
- [x] P1: base_url フィールド追加（Google STT/TTS）
- [x] P2: strip_for_speech import パス整理
- [x] P2: extract_chat_completion_text 共通化（mod.rs）

### 🔄 P2 修正（進行中）
- [ ] run_turn: drain → provider load 順序修正
- [ ] run_caption_turn: prompt drain → provider load 順序修正

### 🔲 Phase 6: ヘッドレス対応（別途検討）
- [ ] Shell（CEF）なしでCore + PlaywrightでMeet参加
- [ ] brain.rs + Shell側の変更が必要
- [ ] 大規模変更のため別ブランチ・別PRで対応

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
