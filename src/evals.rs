#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, Semaphore};

use crate::storage;

const EVAL_SUITE_SCHEMA_VERSION: u32 = 1;
const EVAL_RUN_SCHEMA_VERSION: u32 = 1;
const MAX_CASES: usize = 10_000;
const MAX_PATTERN_CHARS: usize = 4_096;
const MAX_SUITE_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_METADATA_ENTRIES: usize = 128;
const MAX_CASE_TIMEOUT_MS: u64 = 600_000;
pub const REGEX_SYNTAX: &str = "literals, ., ^, $, character ranges, \\d, \\s, \\w, *, +, and ?";

pub type EvalFuture =
    Pin<Box<dyn Future<Output = anyhow::Result<InferenceOutput>> + Send + 'static>>;

pub trait EvalRunner: Send + Sync {
    fn infer(&self, request: InferenceRequest) -> EvalFuture;
}

pub trait EvalProgressSink: Send + Sync {
    fn emit(&self, progress: EvalProgress);
}

impl<F> EvalProgressSink for F
where
    F: Fn(EvalProgress) + Send + Sync,
{
    fn emit(&self, progress: EvalProgress) {
        self(progress);
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EvalSuite {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub cases: Vec<EvalCase>,
}

impl EvalSuite {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_id("suite", &self.id)?;
        if self.name.trim().is_empty() {
            anyhow::bail!("suite name cannot be empty");
        }
        if self.name.len() > 200 || self.description.len() > 10_000 {
            anyhow::bail!("suite name or description is too long");
        }
        if self.cases.is_empty() {
            anyhow::bail!("suite must contain at least one case");
        }
        if self.cases.len() > MAX_CASES {
            anyhow::bail!("suite cannot contain more than {MAX_CASES} cases");
        }
        if self
            .default_model
            .as_deref()
            .is_some_and(|model| model.trim().is_empty())
        {
            anyhow::bail!("default model cannot be empty");
        }

        let mut ids = BTreeSet::new();
        let mut text_bytes = self.name.len().saturating_add(self.description.len());
        for case in &self.cases {
            case.validate(self.default_model.as_deref())?;
            text_bytes = text_bytes.saturating_add(case.prompt.len());
            if !ids.insert(case.id.as_str()) {
                anyhow::bail!("duplicate case id: {}", case.id);
            }
        }
        if text_bytes > MAX_SUITE_TEXT_BYTES {
            anyhow::bail!("suite text exceeds the storage limit");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EvalCase {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub settings: InferenceSettings,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub assertions: Vec<EvalAssertion>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl EvalCase {
    fn validate(&self, default_model: Option<&str>) -> anyhow::Result<()> {
        validate_id("case", &self.id)?;
        if self.prompt.trim().is_empty() {
            anyhow::bail!("case {} has an empty prompt", self.id);
        }
        if self.model.as_deref().or(default_model).is_none() {
            anyhow::bail!("case {} has no model", self.id);
        }
        if self
            .model
            .as_deref()
            .is_some_and(|model| model.trim().is_empty())
        {
            anyhow::bail!("case {} has an empty model", self.id);
        }
        self.settings
            .validate()
            .with_context(|| format!("invalid settings for case {}", self.id))?;
        if self
            .timeout_ms
            .is_some_and(|timeout| !(1..=MAX_CASE_TIMEOUT_MS).contains(&timeout))
        {
            anyhow::bail!(
                "case {} timeout must be between 1 and {MAX_CASE_TIMEOUT_MS} ms",
                self.id
            );
        }
        if self.assertions.is_empty() {
            anyhow::bail!("case {} must contain an assertion", self.id);
        }
        for assertion in &self.assertions {
            assertion
                .validate()
                .with_context(|| format!("invalid assertion for case {}", self.id))?;
        }
        if self.metadata.len() > MAX_METADATA_ENTRIES
            || self
                .metadata
                .iter()
                .any(|(key, value)| key.len() > 1_024 || value.len() > 4_096)
        {
            anyhow::bail!("case {} metadata is too large", self.id);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct InferenceSettings {
    pub temperature: f32,
    pub max_tokens: u32,
    pub seed: Option<u64>,
}

impl Default for InferenceSettings {
    fn default() -> Self {
        Self {
            temperature: 0.0,
            max_tokens: 512,
            seed: Some(0),
        }
    }
}

impl InferenceSettings {
    fn validate(&self) -> anyhow::Result<()> {
        if !self.temperature.is_finite() || !(0.0..=2.0).contains(&self.temperature) {
            anyhow::bail!("temperature must be finite and between 0 and 2");
        }
        if self.max_tokens == 0 {
            anyhow::bail!("max_tokens must be greater than zero");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct InferenceRequest {
    pub suite_id: String,
    pub case_id: String,
    pub model: String,
    pub prompt: String,
    pub settings: InferenceSettings,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct InferenceOutput {
    pub text: String,
    #[serde(default)]
    pub usage: TokenUsage,
    #[serde(default)]
    pub tool_trace: Option<Vec<ToolTraceEntry>>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl TokenUsage {
    pub fn total(self) -> u64 {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolTraceEntry {
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionTarget {
    #[default]
    Output,
    ToolTrace,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvalAssertion {
    Exact {
        #[serde(default)]
        target: AssertionTarget,
        expected: String,
    },
    Contains {
        #[serde(default)]
        target: AssertionTarget,
        expected: String,
    },
    Regex {
        #[serde(default)]
        target: AssertionTarget,
        pattern: String,
    },
    JsonEquals {
        #[serde(default)]
        target: AssertionTarget,
        expected: serde_json::Value,
    },
    JsonShape {
        #[serde(default)]
        target: AssertionTarget,
        shape: JsonShape,
    },
    MaxLatencyMs {
        max: u64,
    },
    MaxTotalTokens {
        max: u64,
    },
}

impl EvalAssertion {
    fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Regex { pattern, .. } => {
                RegexProgram::compile(pattern)?;
            }
            Self::JsonShape { shape, .. } => shape.validate()?,
            Self::MaxLatencyMs { max } if *max == 0 => {
                anyhow::bail!("latency limit must be greater than zero");
            }
            Self::MaxTotalTokens { max } if *max == 0 => {
                anyhow::bail!("token limit must be greater than zero");
            }
            _ => {}
        }
        Ok(())
    }

    fn evaluate(&self, output: &InferenceOutput, latency_ms: u64) -> AssertionResult {
        let (passed, message) = match self {
            Self::Exact { target, expected } => match assertion_text(*target, output) {
                Ok(actual) => (
                    actual == *expected,
                    "target must exactly match the expected value".to_string(),
                ),
                Err(error) => (false, error),
            },
            Self::Contains { target, expected } => match assertion_text(*target, output) {
                Ok(actual) => (
                    actual.contains(expected),
                    "target must contain the expected value".to_string(),
                ),
                Err(error) => (false, error),
            },
            Self::Regex { target, pattern } => match assertion_text(*target, output) {
                Ok(actual) => match RegexProgram::compile(pattern) {
                    Ok(regex) => (
                        regex.is_match(&actual),
                        "target must match the regular expression".to_string(),
                    ),
                    Err(error) => (false, error.to_string()),
                },
                Err(error) => (false, error),
            },
            Self::JsonEquals { target, expected } => match assertion_json(*target, output) {
                Ok(actual) => (
                    actual == *expected,
                    "target JSON must equal the expected value".to_string(),
                ),
                Err(error) => (false, error),
            },
            Self::JsonShape { target, shape } => match assertion_json(*target, output) {
                Ok(actual) => match shape.matches(&actual, "$") {
                    Ok(()) => (
                        true,
                        "target JSON must match the expected shape".to_string(),
                    ),
                    Err(error) => (false, error),
                },
                Err(error) => (false, error),
            },
            Self::MaxLatencyMs { max } => (
                latency_ms <= *max,
                format!("latency must not exceed {max} ms"),
            ),
            Self::MaxTotalTokens { max } => (
                output.usage.total() <= *max,
                format!("total tokens must not exceed {max}"),
            ),
        };
        AssertionResult {
            assertion: self.kind(),
            passed,
            message: if passed {
                "passed".to_string()
            } else {
                message
            },
        }
    }

    fn kind(&self) -> AssertionKind {
        match self {
            Self::Exact { .. } => AssertionKind::Exact,
            Self::Contains { .. } => AssertionKind::Contains,
            Self::Regex { .. } => AssertionKind::Regex,
            Self::JsonEquals { .. } => AssertionKind::JsonEquals,
            Self::JsonShape { .. } => AssertionKind::JsonShape,
            Self::MaxLatencyMs { .. } => AssertionKind::MaxLatencyMs,
            Self::MaxTotalTokens { .. } => AssertionKind::MaxTotalTokens,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionKind {
    Exact,
    Contains,
    Regex,
    JsonEquals,
    JsonShape,
    MaxLatencyMs,
    MaxTotalTokens,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonKind {
    Null,
    Boolean,
    Number,
    String,
    Array,
    Object,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct JsonShape {
    #[serde(default)]
    pub kind: Option<JsonKind>,
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, JsonShape>,
    #[serde(default)]
    pub items: Option<Box<JsonShape>>,
}

impl JsonShape {
    fn validate(&self) -> anyhow::Result<()> {
        let expects_object = self.kind == Some(JsonKind::Object);
        if (!self.required.is_empty() || !self.properties.is_empty()) && !expects_object {
            anyhow::bail!("JSON properties require an object shape");
        }
        if self.items.is_some() && self.kind != Some(JsonKind::Array) {
            anyhow::bail!("JSON items require an array shape");
        }
        let mut required = BTreeSet::new();
        for key in &self.required {
            if key.is_empty() || !required.insert(key) {
                anyhow::bail!("JSON required keys must be non-empty and unique");
            }
        }
        for child in self.properties.values() {
            child.validate()?;
        }
        if let Some(items) = &self.items {
            items.validate()?;
        }
        Ok(())
    }

    fn matches(&self, value: &serde_json::Value, path: &str) -> Result<(), String> {
        if let Some(kind) = self.kind {
            let matches = match kind {
                JsonKind::Null => value.is_null(),
                JsonKind::Boolean => value.is_boolean(),
                JsonKind::Number => value.is_number(),
                JsonKind::String => value.is_string(),
                JsonKind::Array => value.is_array(),
                JsonKind::Object => value.is_object(),
            };
            if !matches {
                return Err(format!("{path} must be {}", json_kind_name(kind)));
            }
        }
        if let Some(object) = value.as_object() {
            for key in &self.required {
                if !object.contains_key(key) {
                    return Err(format!("{path} is missing required key {key}"));
                }
            }
            for (key, shape) in &self.properties {
                if let Some(child) = object.get(key) {
                    shape.matches(child, &format!("{path}.{key}"))?;
                }
            }
        }
        if let (Some(items), Some(values)) = (&self.items, value.as_array()) {
            for (index, child) in values.iter().enumerate() {
                items.matches(child, &format!("{path}[{index}]"))?;
            }
        }
        Ok(())
    }
}

fn json_kind_name(kind: JsonKind) -> &'static str {
    match kind {
        JsonKind::Null => "null",
        JsonKind::Boolean => "a boolean",
        JsonKind::Number => "a number",
        JsonKind::String => "a string",
        JsonKind::Array => "an array",
        JsonKind::Object => "an object",
    }
}

#[derive(Clone, Debug)]
pub struct EvalRunOptions {
    pub max_concurrency: usize,
    pub default_timeout: Duration,
    pub max_capture_bytes: usize,
}

impl Default for EvalRunOptions {
    fn default() -> Self {
        Self {
            max_concurrency: 2,
            default_timeout: Duration::from_secs(120),
            max_capture_bytes: 1024 * 1024,
        }
    }
}

impl EvalRunOptions {
    fn validate(&self) -> anyhow::Result<()> {
        if self.max_concurrency == 0 {
            anyhow::bail!("evaluation concurrency must be greater than zero");
        }
        if self.default_timeout.is_zero() {
            anyhow::bail!("default timeout must be greater than zero");
        }
        if self.max_capture_bytes == 0 {
            anyhow::bail!("capture limit must be greater than zero");
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct EvalCancellation {
    inner: Arc<CancellationInner>,
}

#[derive(Default)]
struct CancellationInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl EvalCancellation {
    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum EvalProgress {
    SuiteStarted {
        total: usize,
    },
    CaseStarted {
        index: usize,
        case_id: String,
    },
    CaseFinished {
        index: usize,
        case_id: String,
        status: CaseStatus,
    },
    SuiteFinished {
        metrics: EvalMetrics,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EvalRun {
    pub id: String,
    pub suite_id: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
    pub results: Vec<CaseResult>,
    pub metrics: EvalMetrics,
    #[serde(default)]
    pub baseline: Option<BaselineComparison>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CaseResult {
    pub case_id: String,
    pub model: String,
    pub status: CaseStatus,
    pub latency_ms: u64,
    #[serde(default)]
    pub usage: Option<TokenUsage>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub tool_trace: Option<Vec<ToolTraceEntry>>,
    #[serde(default)]
    pub assertion_results: Vec<AssertionResult>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseStatus {
    Passed,
    Failed,
    TimedOut,
    Cancelled,
    Error,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct AssertionResult {
    pub assertion: AssertionKind,
    pub passed: bool,
    pub message: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct EvalMetrics {
    pub total_cases: usize,
    pub passed: usize,
    pub failed: usize,
    pub timed_out: usize,
    pub cancelled: usize,
    pub errors: usize,
    pub pass_rate: f64,
    pub latency_cases: usize,
    pub mean_latency_ms: f64,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
    pub tokenized_cases: usize,
    pub total_tokens: u64,
    pub mean_total_tokens: f64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct RegressionThresholds {
    pub max_pass_rate_drop: Option<f64>,
    pub max_p95_latency_increase_percent: Option<f64>,
    pub max_mean_tokens_increase_percent: Option<f64>,
}

impl RegressionThresholds {
    pub fn validate(&self) -> anyhow::Result<()> {
        for (name, value) in [
            ("pass-rate drop", self.max_pass_rate_drop),
            (
                "p95 latency increase",
                self.max_p95_latency_increase_percent,
            ),
            ("mean token increase", self.max_mean_tokens_increase_percent),
        ] {
            if value.is_some_and(|value| !value.is_finite() || value < 0.0) {
                anyhow::bail!("{name} threshold must be finite and non-negative");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BaselineComparison {
    pub baseline_run_id: String,
    pub pass_rate_drop: f64,
    pub p95_latency_increase_percent: Option<f64>,
    pub mean_tokens_increase_percent: Option<f64>,
    pub regressions: Vec<Regression>,
}

impl BaselineComparison {
    pub fn regressed(&self) -> bool {
        !self.regressions.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Regression {
    pub metric: RegressionMetric,
    pub baseline: f64,
    pub current: f64,
    pub change: Option<f64>,
    pub threshold: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegressionMetric {
    PassRate,
    P95Latency,
    MeanTotalTokens,
}

pub async fn run_suite(
    suite: &EvalSuite,
    run_id: impl Into<String>,
    runner: Arc<dyn EvalRunner>,
    options: EvalRunOptions,
    cancellation: EvalCancellation,
    redactor: Redactor,
    progress: Option<Arc<dyn EvalProgressSink>>,
) -> anyhow::Result<EvalRun> {
    suite.validate()?;
    options.validate()?;
    let run_id = run_id.into();
    validate_id("run", &run_id)?;
    let started_at = chrono::Utc::now();
    emit_progress(
        &progress,
        EvalProgress::SuiteStarted {
            total: suite.cases.len(),
        },
    );

    let semaphore = Arc::new(Semaphore::new(options.max_concurrency));
    let mut handles = Vec::with_capacity(suite.cases.len());
    for (index, case) in suite.cases.iter().cloned().enumerate() {
        let suite_id = suite.id.clone();
        let model = case
            .model
            .clone()
            .or_else(|| suite.default_model.clone())
            .expect("validated suite has a model");
        let runner = runner.clone();
        let semaphore = semaphore.clone();
        let cancellation = cancellation.clone();
        let redactor = redactor.clone();
        let progress = progress.clone();
        let default_timeout = options.default_timeout;
        let max_capture_bytes = options.max_capture_bytes;
        handles.push(tokio::spawn(async move {
            let permit = tokio::select! {
                _ = cancellation.cancelled() => None,
                permit = semaphore.acquire_owned() => permit.ok(),
            };
            let Some(permit) = permit else {
                let result = cancelled_result(&case, model, 0);
                emit_case_finished(&progress, index, &result);
                return (index, result);
            };
            emit_progress(
                &progress,
                EvalProgress::CaseStarted {
                    index,
                    case_id: case.id.clone(),
                },
            );
            let started = Instant::now();
            let timeout = case
                .timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(default_timeout);
            let request = InferenceRequest {
                suite_id,
                case_id: case.id.clone(),
                model: model.clone(),
                prompt: case.prompt.clone(),
                settings: case.settings.clone(),
                metadata: case.metadata.clone(),
            };
            let outcome = tokio::select! {
                _ = cancellation.cancelled() => CaseOutcome::Cancelled,
                result = tokio::time::timeout(timeout, runner.infer(request)) => {
                    match result {
                        Ok(Ok(output)) => CaseOutcome::Output(output),
                        Ok(Err(error)) => CaseOutcome::Error(format!("{error:#}")),
                        Err(_) => CaseOutcome::TimedOut,
                    }
                }
            };
            let latency_ms = millis_u64(started.elapsed());
            drop(permit);
            let result = finish_case(
                &case,
                model,
                outcome,
                latency_ms,
                &redactor,
                max_capture_bytes,
            );
            emit_case_finished(&progress, index, &result);
            (index, result)
        }));
    }

    let mut indexed_results = Vec::with_capacity(handles.len());
    let mut handles = handles.into_iter();
    while let Some(handle) = handles.next() {
        match handle.await {
            Ok(result) => indexed_results.push(result),
            Err(error) => {
                cancellation.cancel();
                for remaining in handles {
                    remaining.abort();
                }
                return Err(error).context("evaluation task failed");
            }
        }
    }
    indexed_results.sort_by_key(|(index, _)| *index);
    let results = indexed_results
        .into_iter()
        .map(|(_, result)| result)
        .collect::<Vec<_>>();
    let metrics = aggregate_metrics(&results);
    emit_progress(
        &progress,
        EvalProgress::SuiteFinished {
            metrics: metrics.clone(),
        },
    );
    Ok(EvalRun {
        id: run_id,
        suite_id: suite.id.clone(),
        started_at,
        finished_at: chrono::Utc::now(),
        results,
        metrics,
        baseline: None,
    })
}

pub fn compare_baseline(
    current: &EvalRun,
    baseline: &EvalRun,
    thresholds: &RegressionThresholds,
) -> anyhow::Result<BaselineComparison> {
    thresholds.validate()?;
    if current.suite_id != baseline.suite_id {
        anyhow::bail!("baseline and current run must use the same suite");
    }
    let pass_rate_drop = (baseline.metrics.pass_rate - current.metrics.pass_rate).max(0.0);
    let latency_change = percent_increase(
        baseline.metrics.p95_latency_ms as f64,
        current.metrics.p95_latency_ms as f64,
    );
    let token_change = percent_increase(
        baseline.metrics.mean_total_tokens,
        current.metrics.mean_total_tokens,
    );
    let mut regressions = Vec::new();
    if let Some(threshold) = thresholds.max_pass_rate_drop {
        if pass_rate_drop > threshold {
            regressions.push(Regression {
                metric: RegressionMetric::PassRate,
                baseline: baseline.metrics.pass_rate,
                current: current.metrics.pass_rate,
                change: Some(pass_rate_drop),
                threshold,
            });
        }
    }
    if let Some(threshold) = thresholds.max_p95_latency_increase_percent {
        if exceeds_percent_threshold(
            latency_change,
            baseline.metrics.p95_latency_ms,
            current.metrics.p95_latency_ms,
            threshold,
        ) {
            regressions.push(Regression {
                metric: RegressionMetric::P95Latency,
                baseline: baseline.metrics.p95_latency_ms as f64,
                current: current.metrics.p95_latency_ms as f64,
                change: latency_change,
                threshold,
            });
        }
    }
    if let Some(threshold) = thresholds.max_mean_tokens_increase_percent {
        if exceeds_float_threshold(
            token_change,
            baseline.metrics.mean_total_tokens,
            current.metrics.mean_total_tokens,
            threshold,
        ) {
            regressions.push(Regression {
                metric: RegressionMetric::MeanTotalTokens,
                baseline: baseline.metrics.mean_total_tokens,
                current: current.metrics.mean_total_tokens,
                change: token_change,
                threshold,
            });
        }
    }
    Ok(BaselineComparison {
        baseline_run_id: baseline.id.clone(),
        pass_rate_drop,
        p95_latency_increase_percent: latency_change,
        mean_tokens_increase_percent: token_change,
        regressions,
    })
}

fn exceeds_percent_threshold(
    change: Option<f64>,
    baseline: u64,
    current: u64,
    threshold: f64,
) -> bool {
    change.is_some_and(|value| value > threshold) || (baseline == 0 && current > 0)
}

fn exceeds_float_threshold(
    change: Option<f64>,
    baseline: f64,
    current: f64,
    threshold: f64,
) -> bool {
    change.is_some_and(|value| value > threshold) || (baseline == 0.0 && current > 0.0)
}

fn percent_increase(baseline: f64, current: f64) -> Option<f64> {
    if baseline == 0.0 {
        return (current == 0.0).then_some(0.0);
    }
    Some(((current - baseline) / baseline * 100.0).max(0.0))
}

pub fn aggregate_metrics(results: &[CaseResult]) -> EvalMetrics {
    let mut metrics = EvalMetrics {
        total_cases: results.len(),
        ..EvalMetrics::default()
    };
    let mut latencies = Vec::with_capacity(results.len());
    for result in results {
        match result.status {
            CaseStatus::Passed => metrics.passed += 1,
            CaseStatus::Failed => metrics.failed += 1,
            CaseStatus::TimedOut => metrics.timed_out += 1,
            CaseStatus::Cancelled => metrics.cancelled += 1,
            CaseStatus::Error => metrics.errors += 1,
        }
        if result.status != CaseStatus::Cancelled {
            latencies.push(result.latency_ms);
        }
        if let Some(usage) = result.usage {
            metrics.tokenized_cases += 1;
            metrics.total_tokens = metrics.total_tokens.saturating_add(usage.total());
        }
    }
    if !results.is_empty() {
        metrics.pass_rate = metrics.passed as f64 / results.len() as f64;
    }
    if !latencies.is_empty() {
        metrics.latency_cases = latencies.len();
        metrics.mean_latency_ms =
            latencies.iter().map(|value| *value as f64).sum::<f64>() / latencies.len() as f64;
        latencies.sort_unstable();
        metrics.p50_latency_ms = percentile(&latencies, 50);
        metrics.p95_latency_ms = percentile(&latencies, 95);
    }
    if metrics.tokenized_cases > 0 {
        metrics.mean_total_tokens = metrics.total_tokens as f64 / metrics.tokenized_cases as f64;
    }
    metrics
}

fn percentile(values: &[u64], percentile: usize) -> u64 {
    let index = (values.len() * percentile).div_ceil(100).saturating_sub(1);
    values[index]
}

enum CaseOutcome {
    Output(InferenceOutput),
    TimedOut,
    Cancelled,
    Error(String),
}

fn finish_case(
    case: &EvalCase,
    model: String,
    outcome: CaseOutcome,
    latency_ms: u64,
    redactor: &Redactor,
    max_capture_bytes: usize,
) -> CaseResult {
    match outcome {
        CaseOutcome::Output(mut output) => {
            let assertion_results = case
                .assertions
                .iter()
                .map(|assertion| assertion.evaluate(&output, latency_ms))
                .collect::<Vec<_>>();
            let status = if assertion_results.iter().all(|result| result.passed) {
                CaseStatus::Passed
            } else {
                CaseStatus::Failed
            };
            redactor.redact_output(&mut output);
            let text = truncate_utf8(output.text, max_capture_bytes);
            CaseResult {
                case_id: case.id.clone(),
                model,
                status,
                latency_ms,
                usage: Some(output.usage),
                output: Some(text),
                tool_trace: output.tool_trace,
                assertion_results,
                error: None,
            }
        }
        CaseOutcome::TimedOut => CaseResult {
            case_id: case.id.clone(),
            model,
            status: CaseStatus::TimedOut,
            latency_ms,
            usage: None,
            output: None,
            tool_trace: None,
            assertion_results: Vec::new(),
            error: Some("case exceeded its timeout".to_string()),
        },
        CaseOutcome::Cancelled => cancelled_result(case, model, latency_ms),
        CaseOutcome::Error(error) => CaseResult {
            case_id: case.id.clone(),
            model,
            status: CaseStatus::Error,
            latency_ms,
            usage: None,
            output: None,
            tool_trace: None,
            assertion_results: Vec::new(),
            error: Some(redactor.redact_text(&error)),
        },
    }
}

fn cancelled_result(case: &EvalCase, model: String, latency_ms: u64) -> CaseResult {
    CaseResult {
        case_id: case.id.clone(),
        model,
        status: CaseStatus::Cancelled,
        latency_ms,
        usage: None,
        output: None,
        tool_trace: None,
        assertion_results: Vec::new(),
        error: Some("evaluation cancelled".to_string()),
    }
}

fn emit_case_finished(
    progress: &Option<Arc<dyn EvalProgressSink>>,
    index: usize,
    result: &CaseResult,
) {
    emit_progress(
        progress,
        EvalProgress::CaseFinished {
            index,
            case_id: result.case_id.clone(),
            status: result.status,
        },
    );
}

fn emit_progress(progress: &Option<Arc<dyn EvalProgressSink>>, event: EvalProgress) {
    if let Some(progress) = progress {
        progress.emit(event);
    }
}

fn millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push_str("[truncated]");
    value
}

fn assertion_text(target: AssertionTarget, output: &InferenceOutput) -> Result<String, String> {
    match target {
        AssertionTarget::Output => Ok(output.text.clone()),
        AssertionTarget::ToolTrace => output
            .tool_trace
            .as_ref()
            .ok_or_else(|| "tool trace was not captured".to_string())
            .and_then(|trace| {
                serde_json::to_string(trace)
                    .map_err(|_| "tool trace could not be serialized".to_string())
            }),
    }
}

fn assertion_json(
    target: AssertionTarget,
    output: &InferenceOutput,
) -> Result<serde_json::Value, String> {
    match target {
        AssertionTarget::Output => serde_json::from_str(&output.text)
            .map_err(|_| "model output is not valid JSON".to_string()),
        AssertionTarget::ToolTrace => output
            .tool_trace
            .as_ref()
            .ok_or_else(|| "tool trace was not captured".to_string())
            .and_then(|trace| {
                serde_json::to_value(trace)
                    .map_err(|_| "tool trace could not be serialized".to_string())
            }),
    }
}

#[derive(Clone, Default)]
pub struct Redactor {
    secrets: Arc<Vec<String>>,
}

impl Redactor {
    pub fn new<I, S>(secrets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut secrets = secrets
            .into_iter()
            .map(Into::into)
            .filter(|secret: &String| !secret.is_empty())
            .collect::<Vec<_>>();
        secrets.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        secrets.dedup();
        Self {
            secrets: Arc::new(secrets),
        }
    }

    pub fn redact_text(&self, value: &str) -> String {
        self.secrets.iter().fold(value.to_string(), |text, secret| {
            text.replace(secret, "[REDACTED]")
        })
    }

    fn redact_output(&self, output: &mut InferenceOutput) {
        output.text = self.redact_text(&output.text);
        if let Some(trace) = &mut output.tool_trace {
            for entry in trace {
                entry.name = self.redact_text(&entry.name);
                redact_json_value(&mut entry.arguments, self);
                if let Some(result) = &mut entry.result {
                    redact_json_value(result, self);
                }
                if let Some(error) = &mut entry.error {
                    *error = self.redact_text(error);
                }
            }
        }
    }

    fn redact_run(&self, run: &mut EvalRun) {
        for result in &mut run.results {
            if let Some(output) = &mut result.output {
                *output = self.redact_text(output);
            }
            if let Some(error) = &mut result.error {
                *error = self.redact_text(error);
            }
            if let Some(trace) = &mut result.tool_trace {
                for entry in trace {
                    entry.name = self.redact_text(&entry.name);
                    redact_json_value(&mut entry.arguments, self);
                    if let Some(value) = &mut entry.result {
                        redact_json_value(value, self);
                    }
                    if let Some(error) = &mut entry.error {
                        *error = self.redact_text(error);
                    }
                }
            }
            for assertion in &mut result.assertion_results {
                assertion.message = self.redact_text(&assertion.message);
            }
        }
    }
}

fn redact_json_value(value: &mut serde_json::Value, redactor: &Redactor) {
    match value {
        serde_json::Value::String(text) => *text = redactor.redact_text(text),
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_value(value, redactor);
            }
        }
        serde_json::Value::Object(values) => {
            let entries = std::mem::take(values);
            for (key, mut value) in entries {
                redact_json_value(&mut value, redactor);
                values.insert(redactor.redact_text(&key), value);
            }
        }
        _ => {}
    }
}

pub struct EvalStore {
    root: PathBuf,
}

impl EvalStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn save_suite(&self, suite: &EvalSuite) -> anyhow::Result<()> {
        suite.validate()?;
        storage::save_versioned(
            &self.suite_path(&suite.id)?,
            EVAL_SUITE_SCHEMA_VERSION,
            suite,
        )
    }

    pub fn load_suite(&self, suite_id: &str) -> anyhow::Result<Option<EvalSuite>> {
        let path = self.suite_path(suite_id)?;
        let suite = storage::load_versioned::<EvalSuite, _>(
            &path,
            EVAL_SUITE_SCHEMA_VERSION,
            |_, value| Ok(value),
        )?;
        if let Some(suite) = &suite {
            suite.validate()?;
        }
        Ok(suite)
    }

    pub fn list_suites(&self) -> anyhow::Result<Vec<EvalSuite>> {
        let directory = self.root.join("suites");
        let mut ids = json_stems(&directory)?;
        ids.sort();
        ids.into_iter()
            .map(|id| {
                self.load_suite(&id)?
                    .ok_or_else(|| anyhow::anyhow!("evaluation suite disappeared while listing"))
            })
            .collect()
    }

    pub fn save_run(&self, run: &EvalRun, redactor: &Redactor) -> anyhow::Result<()> {
        validate_id("run", &run.id)?;
        validate_id("suite", &run.suite_id)?;
        let mut sanitized = run.clone();
        redactor.redact_run(&mut sanitized);
        storage::save_versioned(
            &self.run_path(&run.suite_id, &run.id)?,
            EVAL_RUN_SCHEMA_VERSION,
            &sanitized,
        )
    }

    pub fn load_run(&self, suite_id: &str, run_id: &str) -> anyhow::Result<Option<EvalRun>> {
        let path = self.run_path(suite_id, run_id)?;
        storage::load_versioned(&path, EVAL_RUN_SCHEMA_VERSION, |_, value| Ok(value))
    }

    pub fn list_runs(&self, suite_id: &str) -> anyhow::Result<Vec<EvalRun>> {
        validate_id("suite", suite_id)?;
        let directory = self.root.join("results").join(suite_id);
        let mut runs = json_stems(&directory)?
            .into_iter()
            .map(|id| {
                self.load_run(suite_id, &id)?
                    .ok_or_else(|| anyhow::anyhow!("evaluation run disappeared while listing"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        runs.sort_by(|left, right| {
            right
                .finished_at
                .cmp(&left.finished_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(runs)
    }

    fn suite_path(&self, suite_id: &str) -> anyhow::Result<PathBuf> {
        validate_id("suite", suite_id)?;
        Ok(self.root.join("suites").join(format!("{suite_id}.json")))
    }

    fn run_path(&self, suite_id: &str, run_id: &str) -> anyhow::Result<PathBuf> {
        validate_id("suite", suite_id)?;
        validate_id("run", run_id)?;
        Ok(self
            .root
            .join("results")
            .join(suite_id)
            .join(format!("{run_id}.json")))
    }
}

fn json_stems(directory: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error.into()),
    };
    let mut stems = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
            validate_id("evaluation", stem)?;
            stems.push(stem.to_string());
        }
    }
    Ok(stems)
}

pub fn export_json(run: &EvalRun, redactor: &Redactor) -> anyhow::Result<String> {
    let mut sanitized = run.clone();
    redactor.redact_run(&mut sanitized);
    Ok(serde_json::to_string_pretty(&sanitized)?)
}

pub fn export_jsonl(run: &EvalRun, redactor: &Redactor) -> anyhow::Result<String> {
    let mut sanitized = run.clone();
    redactor.redact_run(&mut sanitized);
    let mut lines = Vec::with_capacity(sanitized.results.len() + 1);
    lines.push(serde_json::to_string(&serde_json::json!({
        "record_type": "run",
        "id": sanitized.id,
        "suite_id": sanitized.suite_id,
        "started_at": sanitized.started_at,
        "finished_at": sanitized.finished_at,
        "metrics": sanitized.metrics,
        "baseline": sanitized.baseline,
    }))?);
    for (index, result) in sanitized.results.into_iter().enumerate() {
        lines.push(serde_json::to_string(&serde_json::json!({
            "record_type": "case",
            "index": index,
            "result": result,
        }))?);
    }
    Ok(format!("{}\n", lines.join("\n")))
}

fn validate_id(kind: &str, id: &str) -> anyhow::Result<()> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && id.as_bytes()[0].is_ascii_alphanumeric();
    if !valid {
        anyhow::bail!("{kind} id must use 1-64 safe filename characters");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum RegexQuantifier {
    One,
    Optional,
    ZeroOrMore,
    OneOrMore,
}

#[derive(Clone)]
enum RegexAtom {
    Literal(char),
    Any,
    Digit,
    Word,
    Space,
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

impl RegexAtom {
    fn matches(&self, value: char) -> bool {
        match self {
            Self::Literal(expected) => value == *expected,
            Self::Any => true,
            Self::Digit => value.is_ascii_digit(),
            Self::Word => value.is_alphanumeric() || value == '_',
            Self::Space => value.is_whitespace(),
            Self::Class { negated, ranges } => {
                let contains = ranges
                    .iter()
                    .any(|(start, end)| *start <= value && value <= *end);
                contains != *negated
            }
        }
    }
}

#[derive(Clone)]
struct RegexToken {
    atom: RegexAtom,
    quantifier: RegexQuantifier,
}

struct RegexProgram {
    anchored_start: bool,
    anchored_end: bool,
    tokens: Vec<RegexToken>,
}

impl RegexProgram {
    fn compile(pattern: &str) -> anyhow::Result<Self> {
        if pattern.chars().count() > MAX_PATTERN_CHARS {
            anyhow::bail!("regular expression is too long");
        }
        let mut chars = pattern.chars().peekable();
        let anchored_start = chars.next_if_eq(&'^').is_some();
        let mut tokens = Vec::new();
        let mut anchored_end = false;
        while let Some(value) = chars.next() {
            if value == '$' && chars.peek().is_none() {
                anchored_end = true;
                break;
            }
            let atom = match value {
                '.' => RegexAtom::Any,
                '\\' => parse_escape(chars.next())?,
                '[' => parse_class(&mut chars)?,
                '*' | '+' | '?' => anyhow::bail!("regular expression has a stray quantifier"),
                value => RegexAtom::Literal(value),
            };
            let quantifier = match chars.peek().copied() {
                Some('*') => {
                    chars.next();
                    RegexQuantifier::ZeroOrMore
                }
                Some('+') => {
                    chars.next();
                    RegexQuantifier::OneOrMore
                }
                Some('?') => {
                    chars.next();
                    RegexQuantifier::Optional
                }
                _ => RegexQuantifier::One,
            };
            tokens.push(RegexToken { atom, quantifier });
        }
        Ok(Self {
            anchored_start,
            anchored_end,
            tokens,
        })
    }

    fn is_match(&self, text: &str) -> bool {
        let values = text.chars().collect::<Vec<_>>();
        if self.anchored_start {
            return self.matches_from(&values, 0);
        }
        (0..=values.len()).any(|start| self.matches_from(&values, start))
    }

    fn matches_from(&self, values: &[char], start: usize) -> bool {
        let mut memo = HashMap::new();
        self.match_tokens(values, 0, start, &mut memo)
    }

    fn match_tokens(
        &self,
        values: &[char],
        token_index: usize,
        value_index: usize,
        memo: &mut HashMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(result) = memo.get(&(token_index, value_index)) {
            return *result;
        }
        let result = if token_index == self.tokens.len() {
            !self.anchored_end || value_index == values.len()
        } else {
            let token = &self.tokens[token_index];
            let mut max = value_index;
            while max < values.len() && token.atom.matches(values[max]) {
                max += 1;
            }
            let matched = max - value_index;
            let (minimum, maximum) = match token.quantifier {
                RegexQuantifier::One => (1, matched.min(1)),
                RegexQuantifier::Optional => (0, matched.min(1)),
                RegexQuantifier::ZeroOrMore => (0, max - value_index),
                RegexQuantifier::OneOrMore => (1, max - value_index),
            };
            maximum >= minimum
                && (minimum..=maximum).rev().any(|count| {
                    self.match_tokens(values, token_index + 1, value_index + count, memo)
                })
        };
        memo.insert((token_index, value_index), result);
        result
    }
}

fn parse_escape(value: Option<char>) -> anyhow::Result<RegexAtom> {
    match value {
        Some('d') => Ok(RegexAtom::Digit),
        Some('w') => Ok(RegexAtom::Word),
        Some('s') => Ok(RegexAtom::Space),
        Some(value) => Ok(RegexAtom::Literal(value)),
        None => anyhow::bail!("regular expression ends with an escape"),
    }
}

fn parse_class<I>(chars: &mut std::iter::Peekable<I>) -> anyhow::Result<RegexAtom>
where
    I: Iterator<Item = char>,
{
    let negated = chars.next_if_eq(&'^').is_some();
    let mut values = Vec::new();
    let mut closed = false;
    while let Some(value) = chars.next() {
        if value == ']' && !values.is_empty() {
            closed = true;
            break;
        }
        let value = if value == '\\' {
            chars
                .next()
                .context("regular expression class ends with an escape")?
        } else {
            value
        };
        values.push(value);
    }
    if !closed {
        anyhow::bail!("regular expression has an unclosed character class");
    }
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < values.len() {
        if index + 2 < values.len() && values[index + 1] == '-' {
            if values[index] > values[index + 2] {
                anyhow::bail!("regular expression has a reversed character range");
            }
            ranges.push((values[index], values[index + 2]));
            index += 3;
        } else {
            ranges.push((values[index], values[index]));
            index += 1;
        }
    }
    Ok(RegexAtom::Class { negated, ranges })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use parking_lot::Mutex;

    use super::*;

    #[derive(Clone)]
    enum FakeBehavior {
        Output {
            delay_ms: u64,
            output: InferenceOutput,
        },
        Error(String),
    }

    struct FakeRunner {
        behaviors: BTreeMap<String, FakeBehavior>,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    impl FakeRunner {
        fn new(behaviors: BTreeMap<String, FakeBehavior>) -> Self {
            Self {
                behaviors,
                active: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::Acquire)
        }
    }

    impl EvalRunner for FakeRunner {
        fn infer(&self, request: InferenceRequest) -> EvalFuture {
            let behavior = self.behaviors.get(&request.case_id).unwrap().clone();
            let active_counter = self.active.clone();
            let peak = self.peak.clone();
            Box::pin(async move {
                struct ActiveGuard(Arc<AtomicUsize>);
                impl Drop for ActiveGuard {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                let active = active_counter.fetch_add(1, Ordering::AcqRel) + 1;
                peak.fetch_max(active, Ordering::AcqRel);
                let _guard = ActiveGuard(active_counter);
                match behavior {
                    FakeBehavior::Output { delay_ms, output } => {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        Ok(output)
                    }
                    FakeBehavior::Error(error) => anyhow::bail!(error),
                }
            })
        }
    }

    fn output(text: &str, total_tokens: u64) -> InferenceOutput {
        InferenceOutput {
            text: text.to_string(),
            usage: TokenUsage {
                prompt_tokens: total_tokens / 2,
                completion_tokens: total_tokens - total_tokens / 2,
            },
            tool_trace: None,
        }
    }

    fn case(id: &str, expected: &str) -> EvalCase {
        EvalCase {
            id: id.to_string(),
            prompt: format!("prompt for {id}"),
            model: None,
            settings: InferenceSettings::default(),
            timeout_ms: None,
            assertions: vec![EvalAssertion::Exact {
                target: AssertionTarget::Output,
                expected: expected.to_string(),
            }],
            metadata: BTreeMap::new(),
        }
    }

    fn suite(cases: Vec<EvalCase>) -> EvalSuite {
        EvalSuite {
            id: "suite-a".to_string(),
            name: "Suite A".to_string(),
            description: String::new(),
            default_model: Some("model-a".to_string()),
            cases,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kiralm-evals-{name}-{}", uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn runs_pass_fail_timeout_and_error_with_stable_order() {
        let mut timeout_case = case("timeout", "never");
        timeout_case.timeout_ms = Some(10);
        let cases = vec![
            case("slow-pass", "yes"),
            case("fast-fail", "yes"),
            timeout_case,
            case("error", "yes"),
        ];
        let runner = Arc::new(FakeRunner::new(BTreeMap::from([
            (
                "slow-pass".to_string(),
                FakeBehavior::Output {
                    delay_ms: 30,
                    output: output("yes", 8),
                },
            ),
            (
                "fast-fail".to_string(),
                FakeBehavior::Output {
                    delay_ms: 1,
                    output: output("no", 4),
                },
            ),
            (
                "timeout".to_string(),
                FakeBehavior::Output {
                    delay_ms: 100,
                    output: output("never", 2),
                },
            ),
            (
                "error".to_string(),
                FakeBehavior::Error("backend failed".to_string()),
            ),
        ])));

        let run = run_suite(
            &suite(cases),
            "run-a",
            runner.clone(),
            EvalRunOptions {
                max_concurrency: 2,
                default_timeout: Duration::from_secs(1),
                max_capture_bytes: 1024,
            },
            EvalCancellation::default(),
            Redactor::default(),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            run.results
                .iter()
                .map(|result| result.case_id.as_str())
                .collect::<Vec<_>>(),
            vec!["slow-pass", "fast-fail", "timeout", "error"]
        );
        assert_eq!(
            run.results
                .iter()
                .map(|result| result.status)
                .collect::<Vec<_>>(),
            vec![
                CaseStatus::Passed,
                CaseStatus::Failed,
                CaseStatus::TimedOut,
                CaseStatus::Error,
            ]
        );
        assert_eq!(runner.peak(), 2);
        assert_eq!(run.metrics.passed, 1);
        assert_eq!(run.metrics.failed, 1);
        assert_eq!(run.metrics.timed_out, 1);
        assert_eq!(run.metrics.errors, 1);
    }

    #[tokio::test]
    async fn cancellation_stops_running_and_queued_cases() {
        let cases = vec![case("one", "done"), case("two", "done")];
        let runner = Arc::new(FakeRunner::new(BTreeMap::from([
            (
                "one".to_string(),
                FakeBehavior::Output {
                    delay_ms: 500,
                    output: output("done", 1),
                },
            ),
            (
                "two".to_string(),
                FakeBehavior::Output {
                    delay_ms: 500,
                    output: output("done", 1),
                },
            ),
        ])));
        let cancellation = EvalCancellation::default();
        let cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            run_suite(
                &suite(cases),
                "run-cancel",
                runner,
                EvalRunOptions {
                    max_concurrency: 1,
                    default_timeout: Duration::from_secs(2),
                    max_capture_bytes: 1024,
                },
                cancel,
                Redactor::default(),
                None,
            )
            .await
            .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancellation.cancel();
        let run = task.await.unwrap();

        assert!(run
            .results
            .iter()
            .all(|result| result.status == CaseStatus::Cancelled));
        assert_eq!(run.metrics.cancelled, 2);
    }

    #[tokio::test]
    async fn evaluates_text_json_shape_latency_tokens_and_tool_trace() {
        let shape = JsonShape {
            kind: Some(JsonKind::Object),
            required: vec!["answer".to_string()],
            properties: BTreeMap::from([(
                "answer".to_string(),
                JsonShape {
                    kind: Some(JsonKind::String),
                    ..JsonShape::default()
                },
            )]),
            items: None,
        };
        let eval_case = EvalCase {
            assertions: vec![
                EvalAssertion::Contains {
                    target: AssertionTarget::Output,
                    expected: "answer".to_string(),
                },
                EvalAssertion::Regex {
                    target: AssertionTarget::Output,
                    pattern: r#"^\{"answer":"[a-z]+"\}$"#.to_string(),
                },
                EvalAssertion::JsonEquals {
                    target: AssertionTarget::Output,
                    expected: serde_json::json!({"answer": "yes"}),
                },
                EvalAssertion::JsonShape {
                    target: AssertionTarget::Output,
                    shape,
                },
                EvalAssertion::Contains {
                    target: AssertionTarget::ToolTrace,
                    expected: "lookup".to_string(),
                },
                EvalAssertion::MaxLatencyMs { max: 100 },
                EvalAssertion::MaxTotalTokens { max: 20 },
            ],
            ..case("all", "unused")
        };
        let mut value = output(r#"{"answer":"yes"}"#, 12);
        value.tool_trace = Some(vec![ToolTraceEntry {
            name: "lookup".to_string(),
            arguments: serde_json::json!({"q": "yes"}),
            result: Some(serde_json::json!({"found": true})),
            error: None,
            duration_ms: Some(2),
        }]);
        let runner = Arc::new(FakeRunner::new(BTreeMap::from([(
            "all".to_string(),
            FakeBehavior::Output {
                delay_ms: 1,
                output: value,
            },
        )])));

        let run = run_suite(
            &suite(vec![eval_case]),
            "run-assertions",
            runner,
            EvalRunOptions::default(),
            EvalCancellation::default(),
            Redactor::default(),
            None,
        )
        .await
        .unwrap();

        assert_eq!(run.results[0].status, CaseStatus::Passed);
        assert!(run.results[0]
            .assertion_results
            .iter()
            .all(|result| result.passed));
    }

    #[test]
    fn validates_suite_ids_models_cases_and_regexes() {
        let mut invalid = suite(vec![case("duplicate", "yes"), case("duplicate", "yes")]);
        assert!(invalid
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
        invalid.cases.truncate(1);
        invalid.cases[0].assertions = vec![EvalAssertion::Regex {
            target: AssertionTarget::Output,
            pattern: "[broken".to_string(),
        }];
        assert!(invalid
            .validate()
            .unwrap_err()
            .to_string()
            .contains("invalid assertion"));
        invalid.cases[0].assertions = vec![EvalAssertion::Contains {
            target: AssertionTarget::Output,
            expected: "x".to_string(),
        }];
        invalid.id = "../escape".to_string();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn compares_aggregate_baselines_against_thresholds() {
        let mut baseline = stored_run("baseline");
        baseline.metrics.pass_rate = 1.0;
        baseline.metrics.p95_latency_ms = 100;
        baseline.metrics.mean_total_tokens = 10.0;
        let mut current = stored_run("current");
        current.metrics.pass_rate = 0.8;
        current.metrics.p95_latency_ms = 130;
        current.metrics.mean_total_tokens = 12.0;

        let comparison = compare_baseline(
            &current,
            &baseline,
            &RegressionThresholds {
                max_pass_rate_drop: Some(0.1),
                max_p95_latency_increase_percent: Some(20.0),
                max_mean_tokens_increase_percent: Some(25.0),
            },
        )
        .unwrap();

        assert!(comparison.regressed());
        assert_eq!(comparison.regressions.len(), 2);
        assert!((comparison.pass_rate_drop - 0.2).abs() < f64::EPSILON);
        assert_eq!(comparison.p95_latency_increase_percent, Some(30.0));
        assert_eq!(comparison.mean_tokens_increase_percent, Some(20.0));
    }

    #[test]
    fn stores_versioned_suites_and_rejects_corrupt_or_future_data() {
        let root = temp_dir("storage");
        let store = EvalStore::new(&root);
        let valid_suite = suite(vec![case("one", "yes")]);
        store.save_suite(&valid_suite).unwrap();
        assert_eq!(store.load_suite("suite-a").unwrap(), Some(valid_suite));

        let corrupt_path = store.suite_path("corrupt").unwrap();
        std::fs::create_dir_all(corrupt_path.parent().unwrap()).unwrap();
        std::fs::write(&corrupt_path, "not-json").unwrap();
        assert!(store.load_suite("corrupt").is_err());

        let future_path = store.suite_path("future").unwrap();
        std::fs::write(
            &future_path,
            serde_json::json!({
                "schema_version": 2,
                "id": "future",
                "name": "Future",
                "default_model": "model-a",
                "cases": []
            })
            .to_string(),
        )
        .unwrap();
        let error = store.load_suite("future").unwrap_err().to_string();
        assert!(error.contains("supports up to 1"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn redacts_outputs_errors_tool_values_storage_and_exports() {
        let secret = "secret-value";
        let mut traced = output(&format!("answer {secret}"), 4);
        traced.tool_trace = Some(vec![ToolTraceEntry {
            name: "tool".to_string(),
            arguments: serde_json::json!({"token": secret}),
            result: Some(serde_json::json!([secret])),
            error: Some(format!("failed with {secret}")),
            duration_ms: None,
        }]);
        let cases = vec![
            case("output", &format!("answer {secret}")),
            case("error", "x"),
        ];
        let runner = Arc::new(FakeRunner::new(BTreeMap::from([
            (
                "output".to_string(),
                FakeBehavior::Output {
                    delay_ms: 0,
                    output: traced,
                },
            ),
            (
                "error".to_string(),
                FakeBehavior::Error(format!("backend exposed {secret}")),
            ),
        ])));
        let redactor = Redactor::new([secret]);
        let run = run_suite(
            &suite(cases),
            "run-redact",
            runner,
            EvalRunOptions::default(),
            EvalCancellation::default(),
            redactor.clone(),
            None,
        )
        .await
        .unwrap();
        let json = export_json(&run, &redactor).unwrap();
        let jsonl = export_jsonl(&run, &redactor).unwrap();
        assert!(!json.contains(secret));
        assert!(!jsonl.contains(secret));
        assert!(json.contains("[REDACTED]"));
        for line in jsonl.lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }

        let root = temp_dir("redacted-storage");
        let store = EvalStore::new(&root);
        store.save_run(&run, &redactor).unwrap();
        let raw =
            std::fs::read_to_string(store.run_path("suite-a", "run-redact").unwrap()).unwrap();
        assert!(!raw.contains(secret));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn regex_subset_supports_anchors_classes_and_quantifiers() {
        let regex = RegexProgram::compile(r"^item-[a-z]+-\d\d?$").unwrap();
        assert!(regex.is_match("item-blue-42"));
        assert!(regex.is_match("item-x-7"));
        assert!(!regex.is_match("an-item-blue-42"));
        assert!(!regex.is_match("item-42"));
        assert!(!RegexProgram::compile("missing").unwrap().is_match(""));
    }

    #[tokio::test]
    async fn emits_progress_for_started_and_finished_cases() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_for_sink = events.clone();
        let sink: Arc<dyn EvalProgressSink> = Arc::new(move |event| {
            events_for_sink.lock().push(event);
        });
        let runner = Arc::new(FakeRunner::new(BTreeMap::from([(
            "one".to_string(),
            FakeBehavior::Output {
                delay_ms: 0,
                output: output("yes", 2),
            },
        )])));

        run_suite(
            &suite(vec![case("one", "yes")]),
            "run-progress",
            runner,
            EvalRunOptions::default(),
            EvalCancellation::default(),
            Redactor::default(),
            Some(sink),
        )
        .await
        .unwrap();

        let events = events.lock();
        assert!(matches!(
            events.first(),
            Some(EvalProgress::SuiteStarted { total: 1 })
        ));
        assert!(matches!(
            events.get(1),
            Some(EvalProgress::CaseStarted { index: 0, .. })
        ));
        assert!(matches!(
            events.get(2),
            Some(EvalProgress::CaseFinished { index: 0, .. })
        ));
        assert!(matches!(
            events.last(),
            Some(EvalProgress::SuiteFinished { .. })
        ));
    }

    fn stored_run(id: &str) -> EvalRun {
        let now = chrono::Utc::now();
        EvalRun {
            id: id.to_string(),
            suite_id: "suite-a".to_string(),
            started_at: now,
            finished_at: now,
            results: Vec::new(),
            metrics: EvalMetrics::default(),
            baseline: None,
        }
    }

    #[test]
    fn store_round_trips_redacted_run_results() {
        let root = temp_dir("run-storage");
        let store = EvalStore::new(&root);
        let run = stored_run("run-a");
        store.save_run(&run, &Redactor::default()).unwrap();
        assert_eq!(store.load_run("suite-a", "run-a").unwrap(), Some(run));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lists_suites_by_id_and_runs_by_completion_time() {
        let root = temp_dir("listing");
        let store = EvalStore::new(&root);
        let mut suite_b = suite(vec![case("one", "yes")]);
        suite_b.id = "suite-b".to_string();
        let mut suite_a = suite(vec![case("one", "yes")]);
        suite_a.id = "suite-a".to_string();
        store.save_suite(&suite_b).unwrap();
        store.save_suite(&suite_a).unwrap();

        let mut older = stored_run("z-random-id");
        older.finished_at = chrono::Utc::now() - chrono::Duration::minutes(5);
        let newer = stored_run("a-random-id");
        store.save_run(&older, &Redactor::default()).unwrap();
        store.save_run(&newer, &Redactor::default()).unwrap();

        assert_eq!(
            store
                .list_suites()
                .unwrap()
                .into_iter()
                .map(|suite| suite.id)
                .collect::<Vec<_>>(),
            vec!["suite-a", "suite-b"]
        );
        assert_eq!(
            store
                .list_runs("suite-a")
                .unwrap()
                .into_iter()
                .map(|run| run.id)
                .collect::<Vec<_>>(),
            vec!["a-random-id", "z-random-id"]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn output_capture_truncates_on_a_utf8_boundary() {
        assert_eq!(truncate_utf8("ééé".to_string(), 3), "é[truncated]");
    }

    #[test]
    fn suite_paths_stay_under_the_evaluation_root() {
        let root = Path::new("/tmp/evals");
        let store = EvalStore::new(root);
        assert!(store.suite_path("../outside").is_err());
        assert_eq!(
            store.suite_path("safe-id").unwrap(),
            root.join("suites/safe-id.json")
        );
    }
}
