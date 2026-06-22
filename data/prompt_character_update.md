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
以下は小説本文です。本文の描写からキャラクター設定として確定的に読み取れる情報を、登場する全キャラクター（新規キャラクターを含む）について抽出してください。

# 制約

- 属性の値は "role", "appearance", "personality", "expression", "background", "relationship", "weakness", "style" の中からのみ選ぶこと
- 1キャラクターについて複数の属性を抽出してよい
- 各属性について複数の内容を抽出してよい
- text には、既存設定を置き換える完全版ではなく、本文から新たに読み取れた追加・補足情報だけを書くこと
- 既存設定に含まれない情報が断片的にしか無い場合も、既存設定を削除せず追加情報として扱うこと
- 本文に新しく登場したキャラクターも対象に含めること
- 本文から読み取れない属性は推測で埋めず、ただ無いとして扱う
- 既存の設定が無くなっている場合、ただ描写されていない可能性も考慮する
- 既存の設定と矛盾する設定があっても、ただ以前に描写されていない可能性も考慮する
- 抽出が何もない場合は updates を空配列で返す

# 本文断片

{{TEXT}}
