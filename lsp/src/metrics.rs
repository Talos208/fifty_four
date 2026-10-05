//! OTel メトリクスの定義と記録用ヘルパ。
//!
//! `otel` feature が無効なら全関数が空実装になるので、呼び出し側は cfg を書かなくてよい。
//! 属性には URI・ファイル名・本文を入れない(cardinality が爆発する)。`provider`/`model`/
//! `rule_code` のような設定・コード由来の有界な値だけを使う。背景は `docs/observability.md`。

#[cfg(feature = "otel")]
mod imp {
    use opentelemetry::{
        KeyValue, global,
        metrics::{Counter, Histogram},
    };
    use std::sync::LazyLock;

    /// LLM の応答待ちは数秒〜数十秒に分布するため、既定の境界(最大10秒)では足りない。
    const LLM_BOUNDARIES: &[f64] = &[0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 40.0, 80.0];

    struct Instruments {
        llm_requests: Counter<u64>,
        llm_duration: Histogram<f64>,
        llm_tokens: Counter<u64>,
        llm_tool_calls: Counter<u64>,
        lsp_duration: Histogram<f64>,
        quality_duration: Histogram<f64>,
        quality_findings: Counter<u64>,
        quality_llm_paragraphs: Counter<u64>,
        quality_llm_skipped: Counter<u64>,
    }

    // `Logger::new` が `set_meter_provider` した後の初回利用時に生成される。
    static M: LazyLock<Instruments> = LazyLock::new(|| {
        let meter = global::meter("fifty_four");
        Instruments {
            llm_requests: meter.u64_counter("fifty_four.llm.requests").build(),
            llm_duration: meter
                .f64_histogram("fifty_four.llm.request.duration")
                .with_unit("s")
                .with_boundaries(LLM_BOUNDARIES.to_vec())
                .build(),
            llm_tokens: meter.u64_counter("fifty_four.llm.tokens").build(),
            llm_tool_calls: meter.u64_counter("fifty_four.llm.tool_calls").build(),
            lsp_duration: meter
                .f64_histogram("fifty_four.lsp.request.duration")
                .with_unit("s")
                .build(),
            quality_duration: meter
                .f64_histogram("fifty_four.quality.analysis.duration")
                .with_unit("s")
                .build(),
            quality_findings: meter.u64_counter("fifty_four.quality.findings").build(),
            quality_llm_paragraphs: meter
                .u64_counter("fifty_four.quality.llm_review.paragraphs")
                .build(),
            quality_llm_skipped: meter
                .u64_counter("fifty_four.quality.llm_review.skipped_running")
                .build(),
        }
    });

    fn kv(k: &'static str, v: &str) -> KeyValue {
        KeyValue::new(k, v.to_string())
    }

    pub fn llm_request(provider: &str, model: &str, outcome: &'static str, secs: f64) {
        let attrs = [kv("provider", provider), kv("model", model)];
        M.llm_duration.record(secs, &attrs);
        M.llm_requests.add(
            1,
            &[
                kv("provider", provider),
                kv("model", model),
                kv("outcome", outcome),
            ],
        );
    }

    pub fn llm_tokens(provider: &str, model: &str, kind: &'static str, n: i32) {
        // API が返さない(None)・0 は記録しない。
        if n > 0 {
            M.llm_tokens.add(
                n as u64,
                &[
                    kv("provider", provider),
                    kv("model", model),
                    kv("type", kind),
                ],
            );
        }
    }

    pub fn llm_tool_call(tool: &str) {
        M.llm_tool_calls.add(1, &[kv("tool", tool)]);
    }

    pub fn lsp_request(method: &'static str, secs: f64) {
        M.lsp_duration.record(secs, &[kv("method", method)]);
    }

    pub fn quality_analysis(secs: f64) {
        M.quality_duration.record(secs, &[]);
    }

    pub fn quality_finding(rule_code: &str, source: &'static str) {
        M.quality_findings
            .add(1, &[kv("rule_code", rule_code), kv("source", source)]);
    }

    pub fn quality_llm_paragraphs(result: &'static str, n: u64) {
        if n > 0 {
            M.quality_llm_paragraphs.add(n, &[kv("result", result)]);
        }
    }

    pub fn quality_llm_skipped_running() {
        M.quality_llm_skipped.add(1, &[]);
    }
}

#[cfg(not(feature = "otel"))]
mod imp {
    pub fn llm_request(_provider: &str, _model: &str, _outcome: &'static str, _secs: f64) {}
    pub fn llm_tokens(_provider: &str, _model: &str, _kind: &'static str, _n: i32) {}
    pub fn llm_tool_call(_tool: &str) {}
    pub fn lsp_request(_method: &'static str, _secs: f64) {}
    pub fn quality_analysis(_secs: f64) {}
    pub fn quality_finding(_rule_code: &str, _source: &'static str) {}
    pub fn quality_llm_paragraphs(_result: &'static str, _n: u64) {}
    pub fn quality_llm_skipped_running() {}
}

pub use imp::*;
