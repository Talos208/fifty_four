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

`capabilities`(サンプルの `deferred.capabilities` を参照)は `structured_output` / `tool_calling` /
`reasoning_effort` / `stop_sequences` の4値を指定でき、指定すると provider ごとの自動導出結果を
完全に置き換える。xAI (`grok-4.20-0309-reasoning` 等)はモデルによって `reasoning_effort` 非対応で
明示しないと 400 になることがあるので、対応表は [lsp-handlers.md](lsp-handlers.md) の
「xAI (Grok) の `reasoning_effort` 対応」を参照。

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

## 見出し行のスタイル上書き(semantic_token_rules)

見出し行の装飾(`.md`の`# `= `type`、`## `以降 = `class`、[lsp-handlers.md](lsp-handlers.md)の
「見出し行の装飾」参照)は、テーマによって `type`/`class` が同じ色・太さで表示され区別がつかない
ことがある。これは Zed の `semantic_token_rules`(`token_type` を指定して `font_weight` /
`font_style` / `foreground_color` をテーマに関係なく上書きできる仕組み)で確実に解決できる。

**`extension/languages/fiftyfour/semantic_token_rules.json` に同梱済み**なので、通常は
settings.json をいじる必要は無い(拡張機能を再インストール/リロードすれば有効になる)。
FiftyFour 言語専用のルールとして適用され、他の言語・LSP サーバーには影響しない
(Zed の拡張ローダーが `<extension>/languages/<言語>/semantic_token_rules.json` を自動で
読み込み、`languages.FiftyFour` 専用のルールとして登録する仕組み。設定ファイルの場所を
変えるだけで settings.json 側の記述は不要)。

```json
{
  "rules": [
    { "token_type": "type", "font_weight": "bold" },
    { "token_type": "class", "font_style": "italic" }
  ]
}
```

自分の環境だけ一時的に上書きしたい場合(拡張機能をいじらず試したい場合)は、settings.json の
`global_lsp_settings.semantic_token_rules` に同じ形で書いても良い(ただし全言語に効く。
`"foreground_color": "#rrggbb"` のような色指定も可能)。両方が定義されている場合の優先順位は
未確認なので、基本的にはどちらか一方だけを使うこと。
