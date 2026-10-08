---
schema: >
  {"type":"object","properties":{"candidates":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"},"confidence":{"type":"number"}},"required":["text","confidence"],"additionalProperties":false},"minItems":1,"maxItems":3}},"required":["candidates"],"additionalProperties":false}
schema_name: fill_mark_candidates
# 候補は「※」に当てはまる語のみ(前後の文は含めない)なので短い。JSON化のオーバーヘッド込みで余裕を持たせる
max_tokens: 512
---
# 指示
対象テキスト中の「※」に当てはまる語の候補を3つ挙げよ。各候補は「※」1文字と置き換わる語そのものだけを含み、前後の文や句読点を含めてはならない。
各候補には `confidence`(0.0〜1.0 の小数)を付けよ。文脈との整合性などから見て、その候補が著者に採用される見込みの高さを正直に自己評価した値とし、候補間で差をつけること(1.0 は「ほぼ確実」、0.0 は「ほぼ採用されない」)。

# 参考情報（必要な場合のみ取得してよい）
続きの内容を判断するうえでプロットや人物設定を確認したい場合は、次のツールを使ってよい（不要なら呼ばなくてよい）。取得した情報は候補を選ぶ判断にのみ用いる。
- `character_info`: 場面に登場する人物の設定（口調・性格・関係性など）を取得する
- `plot_info`: この章（chapter_name は「{{CHAPTER}}」）のプロットや伏線を取得する。
プロットは大筋の方向性を外さないための補助にすぎず、プロットにある出来事・伏線・結末を先取りして書いてはならない。プロットの内容を無理に候補へ織り込まず、直前の文脈から自然に続く候補を優先せよ。
現在{{CHAPTER}}の章を執筆している。{{PROGRESS}}

# 禁止事項
- 「※」を候補に含めること
- 語の前後に文や句読点を付け加えること
- 候補の意図や狙いの説明

出力は `{"candidates": [{"text": "語1", "confidence": 0.8}, {"text": "語2", "confidence": 0.5}, {"text": "語3", "confidence": 0.3}]}` という JSON のみとし、JSON 以外の文字を一切含めないこと。

# 著者は以下の質問をしている。候補を作る参考にしてもよい
{{CHAT}}

# 直前の文脈
{{TEXT}}

# 対象テキスト（この中の「※」に当てはまる語を考えよ）
{{TARGET}}