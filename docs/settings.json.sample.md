Zed の settings.json(`%APPDATA%\Zed\settings.json`)に書く設定のサンプル。
`character_updater` / `llm` は必ず `lsp.fifty-four.initialization_options` の下にネストすること
(直下に書くと Zed が黙って無視し、LLM が `NotInitialized` になる)。
詳細は [lsp-handlers.md](lsp-handlers.md) の「初期化オプション」を参照。

```json
{
  "lsp": {
    "fifty-four": {
      "binary": {
        "path": "C:\\path\\to\\fifty_four_lsp.exe"
      },
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
  },
  "languages": {
    "FiftyFour": {
      "language_servers": ["fifty-four"]
    }
  }
}
```
