次のテキストは途中までの文章である。続きとしてふさわしい、文の候補を3つ挙げよ。候補は1つの文だけを含み、文末を除いて途中に句点があってはならない。文体や、文の長さはテキストに倣え。候補はふさわしさの順に並べよ。
回答は次のJSON schemaに厳密にしたがって生成せよ。

JSON Schema:
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "array",
  "items": {
    "type": "object",
    "properties": {
      "sentence": {
        "type": "string",
        "description": "文の候補"
      },
      "score": {
        "type": "number",
        "description": "ふさわしさのスコア"
      }
    }
  }
}

最終応答は、"["で始まり"]"で終わるJSONのみを出力し、JSON以外の文字は一切応答に含めないこと。

テキスト：
