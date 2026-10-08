---
schema: >
  {"type":"object","properties":{"candidates":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"},"confidence":{"type":"number"}},"required":["text","confidence"],"additionalProperties":false},"minItems":1,"maxItems":3}},"required":["candidates"],"additionalProperties":false}
schema_name: completion_candidates
# 3候補×1文。実測(completion_candidates)の1応答最大413字 ≒620tok に対し余裕を持たせる
max_tokens: 2048
---
# 指示
直前の文の続きとしてふさわしい次の文の候補を3つ挙げよ。候補は1つの文だけを含み、途中に句点があってはならない。

# 参考情報（必要な場合のみ取得してよい）
続きの内容を判断するうえでプロットや人物設定を確認したい場合は、次のツールを使ってよい（不要なら呼ばなくてよい）。取得した情報は候補を選ぶ判断にのみ用いる。
- `character_info`: 場面に登場する人物の設定（口調・性格・関係性など）を取得する
- `plot_info`: この章（chapter_name は「{{CHAPTER}}」）のプロットや伏線を取得する。
プロットは大筋の方向性を外さないための補助にすぎず、プロットにある出来事・伏線・結末を先取りして書いてはならない。プロットの内容を無理に候補へ織り込まず、直前の文脈から自然に続く候補を優先せよ。
現在{{CHAPTER}}の章を執筆している。{{PROGRESS}}

# 禁止事項
- 文末に説明を追加
- 候補の意図や狙いを説明する行（例:「〜への懸念をつなぐ一文」「〜を示す一文」のような、候補そのものではなく候補についての説明）を出力に含めること

各候補には `confidence`(0.0〜1.0 の小数)を付けよ。文脈との整合性などから見て、その候補が著者に採用される見込みの高さを正直に自己評価した値とし、候補間で差をつけること(1.0 は「ほぼ確実」、0.0 は「ほぼ採用されない」)。
出力は `{"candidates": [{"text": "候補1", "confidence": 0.8}, {"text": "候補2", "confidence": 0.5}, {"text": "候補3", "confidence": 0.3}]}` という JSON のみとし、JSON 以外の文字を一切含めないこと。禁止事項は行ってはならない。

# 著者は以下の質問をしている。候補を作る参考にしてもよい
{{CHAT}}

# テキスト
{{TEXT}}