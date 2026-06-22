# 全体アーキテクチャ

## システム構成

```mermaid
flowchart TB
    subgraph Editor["エディタ (Zed)"]
        Zed["Zed Editor"]
        Ext["extension/ (Zed Extension)"]
    end

    subgraph LSP["lsp/ (fifty_four_lsp)"]
        Main["main.rs — Backend (LanguageServer)"]
        HL["highlight.rs — 形態素解析・セマンティックトークン"]
        CC["cursor_context.rs — カーソル文脈分類"]
        LLM["llm.rs — LLM クライアント (genai)"]
        CU["character_updater.rs — キャラ設定自動更新"]
        Types["types.rs — LineData / CachedLinderaToken"]
        Log["logging.rs — OpenTelemetry / tracing"]
        DB["FlightRecorder (debug) — SQLite"]
    end

    subgraph External["外部"]
        Lindera["Lindera (IPADIC)"]
        Providers["LLM プロバイダ\n(Google / OpenAI / Anthropic / xAI / LMStudio)"]
        WS["ワークスペース\n(.txt / .md ファイル)"]
    end

    subgraph Assets["data/ (埋め込みプロンプト)"]
        Prompts["system.md\nprompt_completion*.md\nprompt_character_update.md"]
    end

    Zed --> Ext
    Ext -->|"stdio JSON-RPC"| Main
    Main --> HL
    Main --> CC
    Main --> LLM
    Main --> CU
    Main --> DB
    HL --> Lindera
    LLM --> Providers
    LLM --> Prompts
    CU --> Prompts
    Main --> WS
    CU --> WS
```

## Backend コンポーネント

`main.rs` の `Backend` が LSP の中心。主要フィールドとモジュールの関係:

```mermaid
classDiagram
    class Backend {
        +Client client
        +DashMap text
        +Arc llm (ondemand)
        +Arc background_llm (deferred)
        +Highlighter highlighter
        +CharacterCache character_cache
        +DashMap update_states
        +FlightRecorder db
    }

    class Highlighter {
        +Tokenizer tokenizer (Lindera)
        +tokenize()
        +text_to_lindera_token()
        +to_semantic_tokens()
    }

    class LlmClient {
        <<trait>>
        +chat()
        +add_tool()
        +response_format()
    }

    class CharacterInfoTool {
        +LlmTool
        +キャラ MD 参照
    }

    class UpdateState {
        +dirty_lines
        +accumulated_chars
        +idle_trigger()
    }

    Backend --> Highlighter
    Backend --> LlmClient
    Backend --> UpdateState
    LlmClient <|.. GenericLlmClient
    Backend --> CharacterInfoTool
```

## LLM クライアントの二系統

初期化オプション `llm` から 2 種類のクライアントを構築する。

| クライアント | 設定キー | 用途 |
|---|---|---|
| `llm` | `llm.ondemand` | 文章補完（ユーザー操作に同期） |
| `background_llm` | `llm.deferred`（未指定時は `ondemand` と同じ） | キャラ設定更新（バックグラウンド） |

どちらも `LlmClientBuilder` → `GenericLlmClient` 経由で `genai` クレートを利用する。プロバイダは Google / OpenAI / Anthropic / xAI / LMStudio に対応。

## エージェント（AGENTS.md との対応）

| エージェント | 実装 | 役割 |
|---|---|---|
| LSP サーバエージェント | `main.rs` (`Backend`) | initialize / shutdown / テキスト同期 / 各 LSP ハンドラ |
| 会話ハイライトエージェント | `highlight.rs` (`Highlighter`) | Lindera 形態素解析 → セマンティックトークン生成 |
