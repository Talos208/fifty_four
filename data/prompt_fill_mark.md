---
schema: >
  {"type":"object","properties":{"candidates":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":5}},"required":["candidates"],"additionalProperties":false}
schema_name: fill_mark_candidates
max_tokens: 1024
temperature: 0.8
---
# 指示
対象テキスト中の「※」に当てはまる語の候補を3つ挙げよ。各候補は「※」1文字と置き換わる語そのものだけを含み、前後の文や句読点を含めてはならない。

# 参考情報（必要な場合のみ取得してよい）
語を判断するうえでプロットや人物設定を確認したい場合は、次のツールを使ってよい（不要なら呼ばなくてよい）。
- `plot_info`: この章（chapter_name に「{{CHAPTER}}」を指定）のプロットや伏線を取得する
- `character_info`: 場面に登場する人物の設定（口調・性格・関係性など）を取得する
取得した情報は候補を作る判断にのみ用いること。

# 禁止事項
- 「※」を候補に含めること
- 語の前後に文や句読点を付け加えること
- マークダウン化、番号付け、括弧類での囲み
- 候補の意図や狙いの説明

出力は `{"candidates": ["語1", "語2", "語3"]}` という JSON のみとし、JSON 以外の文字を一切含めないこと。

現在小説の{{CHAPTER}}の章を執筆している。以下の対象テキストには埋まっていない箇所「※」がある。

{{CHAT}}
# 直前の文脈
{{TEXT}}

# 対象テキスト（この中の「※」に当てはまる語を考えよ）
{{TARGET}}