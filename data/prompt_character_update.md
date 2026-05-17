---
schema: >
  {
    "type": "object",
    "properties": {
      "updates": {
        "type": "array",
        "items": {
          "type": "object",
          "properties": {
            "name": { "type": "string" },
            "attribute": {
              "type": "string",
              "enum": ["role", "appearance", "personality", "expression", "background", "relationship", "weakness", "style"]
            },
            "text": { "type": "string" }
          },
          "required": ["name", "attribute", "text"]
        }
      }
    },
    "required": ["updates"]
  }
schema_name: character_updates
max_tokens: 4096
temperature: 0.3
---
以下は小説本文の一部です。既知のキャラクター一覧とその現在の設定を踏まえ、本文の描写からキャラクター属性として確定的に読み取れる情報だけを抽出してください。

# 制約

- 既知キャラクター一覧にいないキャラクターは無視すること
- 属性の値は "role", "appearance", "personality", "expression", "background", "relationship", "weakness", "style" の中からのみ選ぶこと
- 本文から読み取れない属性は含めないこと（推測で埋めないこと）
- 1キャラクターについて複数の属性を抽出してよい
- 既存の設定と矛盾が生じる場合も、本文に書かれた内容を優先して抽出する
- 抽出が何もない場合は updates を空配列で返す

# 既知キャラクター一覧と現在の設定

{{KNOWN_CHARACTERS}}

# 本文断片

{{TEXT}}
