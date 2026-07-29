Zed の settings.json(`%APPDATA%\Zed\settings.json`)に書く設定のサンプル。
`character_updater` / `llm` は必ず `lsp.fifty-four.initialization_options` の下にネストすること
(直下に書くと Zed が黙って無視し、LLM が `NotInitialized` になる)。
詳細は [lsp-handlers.md](lsp-handlers.md) の「初期化オプション」を参照。

`languages.FiftyFour.language_servers` や `lsp.fifty-four.binary.path` は書く必要がない
(拡張のマニフェストによる自動関連付け・バイナリ自動探索が働く。詳細は
[lsp-handlers.md](lsp-handlers.md) の「FiftyFour 言語設定と LSP 起動の最低要件」を参照)。
下記で必須なのは `llm.ondemand`(または `llm.deferred`)と、使用する provider に応じた
API キーの環境変数(`GEMINI_API_KEY` / `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `XAI_API_KEY` 等。
`lmstudio` は認証不要)。API キーは settings.json ではなく OS のユーザー環境変数に設定すること。

```json
{
  "lsp": {
    "fifty-four": {
      "initialization_options": {
        "llm": {
          "ondemand": {
            "provider": "google",
            "model": "gemini-3.1-flash-lite"
          },
          "deferred": {
            "provider": "lmstudio",
            "url": "http://localhost:1234",
            "model": "llm-jp-3.1-1.8b-function-calling",
            "capabilities": ["structured_output", "tool_calling"]
          }
        }
      }
    }
  }
}
```
