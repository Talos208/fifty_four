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
完全に置き換える。加えて、chat_template が system を描画しないモデル向けの `no_system_role`
(system を最初の user メッセージへ入れて送る)も指定できる。LLM-JP-3 系は名前から自動で付く。xAI (`grok-4.20-0309-reasoning` 等)はモデルによって `reasoning_effort` 非対応で
明示しないと 400 になることがあるので、対応表は [lsp-handlers.md](lsp-handlers.md) の
「xAI (Grok) の `reasoning_effort` 対応」を参照。

`lmstudio` は通常 `capabilities` を書く必要はない(起動時に LM Studio 自身と HuggingFace への
問い合わせで自動推定される。詳細は [lsp-handlers.md](lsp-handlers.md) の
「LMStudio の capability 自動推定」を参照)。自動推定を上書きしたいときだけ明示する。

Qwen3 系(`enable_thinking` トグルを持つモデル)は reasoning 制御に `chat_template_kwargs` を使うが、
**現行の LM Studio にはこれを壊すバグがある**(補完が空になる。詳細は [lsp-handlers.md](lsp-handlers.md) の
「Qwen3 系の reasoning 制御と既知の LM Studio 不具合」参照)。影響を受ける場合は
`capabilities` に `"structured_output"`/`"tool_calling"` のみを明示し `"reasoning_effort"` を
含めないことでオプトアウトできる。

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

## 文章品質診断(quality)

`lsp.fifty-four.initialization_options.quality` の下に書く。すべて省略可能で、省略した項目は既定値になる。
診断は `.txt` 原稿が対象(`.md` も対象にするなら `include_md`)。台詞(`「」`内)は、記号の誤用
(`punct-char`)を除いて対象外。

```json
{
  "lsp": {
    "fifty-four": {
      "initialization_options": {
        "quality": {
          "enabled": true,
          "idle_ms": 800,
          "include_md": false,

          "llm_review": false,
          "llm_review_idle_ms": 5000,
          "llm_review_max_chars": 3000,

          "rules": {
            "disabled": ["fragment-run", "grandiose-word"],
            "fragment_max_chars": 10,
            "fragment_run": 3,
            "grandiose_threshold": 3,
            "vocab_extra": { "grandiose_words": ["比類なき"] },
            "stock_phrases": ["重要なのは", "と言えるだろう"]
          }
        }
      }
    }
  }
}
```

- **`llm_review`**(既定 `false`): 決定的ルールでは拾えない「AI が書いたような不自然さ」(場面の壮大化・
  無生物の擬人化・直訳の比喩・段落の閉じ方の均一化など)を LLM に診断させる。`llm.deferred`
  (無ければ `llm.ondemand`)の LLM を使うので、課金・負荷を意識して明示的に有効にすること。
  最後の編集から `llm_review_idle_ms` 経つ、またはファイルを開く・保存すると起動する。未診断の段落
  (本文ハッシュが未登録のもの)だけを送るので、2回目以降は差分しか送らない。LLM の応答待ちの間に入った
  編集は無視され、その段落は次のアイドル時か保存で再診断される。
- **`rules.disabled`**: 無効にするルールコード。コードは診断の `code` と同じ(例 `no-chain`)。未知のコードは無視。
- **`rules.*`(閾値)**: `QualityConfig`(`lsp/src/quality/mod.rs`)のフィールド名がそのままキーになる。
  型が合わない値があると、`rules` 全体が既定値に戻る(警告がログに出る)。
- **`rules.*`(語彙リスト)**: `grandiose_words` / `pseudo_concrete_words` / `stock_phrases` /
  `translationese_phrases` / `abstract_have_nouns` / `inanimate_subjects` / `inanimate_verbs` /
  `intensifiers` / `hedges` は**置き換え**。既定に足すだけなら `vocab_extra`(キーはリスト名)を使う。
  ファンタジーなどで「運命」「残酷」が正当に頻出する作風は、`grandiose_words` を絞るか、
  `grandiose-word` を `disabled` に入れる。

デバッグビルドでは、指摘が `db/fifty_four.db` の `quality_findings`(決定的ルール・LLM とも)、
LLM の要求と生応答が `quality_reviews` に記録される。決定的ルールは前回の診断に無かった指摘だけを記録する。
LLM の抜粋が本文中に見つからなかった指摘は、`message` の末尾が「(位置特定失敗)」になる。
