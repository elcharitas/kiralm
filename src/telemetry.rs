#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::RwLock;
use serde::Serialize;

const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct TelemetryLimits {
    pub max_series: usize,
    pub max_series_per_metric: usize,
    pub max_labels_per_series: usize,
    pub max_metric_name_len: usize,
    pub max_label_name_len: usize,
    pub max_label_value_bytes: usize,
    pub max_operation_bytes: usize,
    pub max_recent_errors: usize,
    pub max_error_message_bytes: usize,
    pub histogram_bounds_seconds: Vec<f64>,
    pub correlation_prefix: Option<String>,
}

impl Default for TelemetryLimits {
    fn default() -> Self {
        Self {
            max_series: 512,
            max_series_per_metric: 128,
            max_labels_per_series: 8,
            max_metric_name_len: 128,
            max_label_name_len: 64,
            max_label_value_bytes: 128,
            max_operation_bytes: 96,
            max_recent_errors: 64,
            max_error_message_bytes: 512,
            histogram_bounds_seconds: vec![
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
            ],
            correlation_prefix: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryError {
    InvalidLimits(String),
    InvalidMetricName(String),
    InvalidLabelName(String),
    TooManyLabels,
    InvalidCorrelationId,
    MetricKindConflict(String),
    NonFiniteGauge,
}

impl fmt::Display for TelemetryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits(message) => {
                write!(formatter, "invalid telemetry limits: {message}")
            }
            Self::InvalidMetricName(name) => write!(formatter, "invalid metric name: {name}"),
            Self::InvalidLabelName(name) => write!(formatter, "invalid label name: {name}"),
            Self::TooManyLabels => formatter.write_str("metric has too many labels"),
            Self::InvalidCorrelationId => formatter.write_str("invalid correlation ID"),
            Self::MetricKindConflict(name) => {
                write!(
                    formatter,
                    "metric kind conflicts with existing series: {name}"
                )
            }
            Self::NonFiniteGauge => formatter.write_str("gauge value must be finite"),
        }
    }
}

impl std::error::Error for TelemetryError {}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct CorrelationId(String);

impl CorrelationId {
    pub fn parse(value: impl Into<String>) -> Result<Self, TelemetryError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 96
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
        {
            return Err(TelemetryError::InvalidCorrelationId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CorrelationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetrySubsystem {
    Request,
    Engine,
    Cache,
    Knowledge,
    Evaluation,
    Attachment,
}

impl TelemetrySubsystem {
    fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Engine => "engine",
            Self::Cache => "cache",
            Self::Knowledge => "knowledge",
            Self::Evaluation => "evaluation",
            Self::Attachment => "attachment",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Success,
    Error,
    Cancelled,
    Rejected,
    Timeout,
}

impl OperationStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::Rejected => "rejected",
            Self::Timeout => "timeout",
        }
    }

    fn is_error(self) -> bool {
        matches!(self, Self::Error | Self::Rejected | Self::Timeout)
    }
}

#[derive(Debug, Clone)]
pub struct OperationRecord<'a> {
    pub subsystem: TelemetrySubsystem,
    pub operation: &'a str,
    pub status: OperationStatus,
    pub duration: Option<Duration>,
    pub correlation_id: Option<&'a CorrelationId>,
    pub error: Option<&'a str>,
}

impl<'a> OperationRecord<'a> {
    pub fn new(subsystem: TelemetrySubsystem, operation: &'a str, status: OperationStatus) -> Self {
        Self {
            subsystem,
            operation,
            status,
            duration: None,
            correlation_id: None,
            error: None,
        }
    }

    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }

    pub fn with_correlation_id(mut self, correlation_id: &'a CorrelationId) -> Self {
        self.correlation_id = Some(correlation_id);
        self
    }

    pub fn with_error(mut self, error: &'a str) -> Self {
        self.error = Some(error);
        self
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordResult {
    pub recorded_series: usize,
    pub dropped_series: usize,
}

impl RecordResult {
    fn recorded() -> Self {
        Self {
            recorded_series: 1,
            dropped_series: 0,
        }
    }

    fn dropped() -> Self {
        Self {
            recorded_series: 0,
            dropped_series: 1,
        }
    }

    fn merge(&mut self, other: Self) {
        self.recorded_series += other.recorded_series;
        self.dropped_series += other.dropped_series;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthComponent {
    Daemon,
    Engine,
    Cache,
    Storage,
    Knowledge,
    Evaluation,
    Attachments,
}

impl HealthComponent {
    fn as_str(self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::Engine => "engine",
            Self::Cache => "cache",
            Self::Storage => "storage",
            Self::Knowledge => "knowledge",
            Self::Evaluation => "evaluation",
            Self::Attachments => "attachments",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthInput<'a> {
    pub component: HealthComponent,
    pub status: HealthStatus,
    pub ready: bool,
    pub required: bool,
    pub detail: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthComponentSnapshot {
    pub component: HealthComponent,
    pub status: HealthStatus,
    pub ready: bool,
    pub required: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthSummary {
    pub status: HealthStatus,
    pub ready: bool,
    pub components: Vec<HealthComponentSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CounterSnapshot {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub value: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GaugeSnapshot {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistogramSnapshot {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub bounds_seconds: Vec<f64>,
    pub cumulative_counts: Vec<u64>,
    pub count: u64,
    pub sum_seconds: f64,
    pub min_seconds: Option<f64>,
    pub max_seconds: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecentError {
    pub sequence: u64,
    pub subsystem: TelemetrySubsystem,
    pub operation: String,
    pub correlation_id: Option<CorrelationId>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TelemetryStats {
    pub series: usize,
    pub dropped_series: u64,
    pub saturated_counters: u64,
    pub total_errors: u64,
    pub evicted_errors: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TelemetrySnapshot {
    pub schema_version: u32,
    pub counters: Vec<CounterSnapshot>,
    pub gauges: Vec<GaugeSnapshot>,
    pub histograms: Vec<HistogramSnapshot>,
    pub health: HealthSummary,
    pub recent_errors: Vec<RecentError>,
    pub stats: TelemetryStats,
}

impl TelemetrySnapshot {
    pub fn to_stable_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MetricKey {
    name: String,
    labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct HistogramValue {
    bucket_counts: Vec<u64>,
    count: u64,
    sum_nanoseconds: u128,
    min_seconds: Option<f64>,
    max_seconds: Option<f64>,
}

impl HistogramValue {
    fn new(bucket_count: usize) -> Self {
        Self {
            bucket_counts: vec![0; bucket_count],
            count: 0,
            sum_nanoseconds: 0,
            min_seconds: None,
            max_seconds: None,
        }
    }

    fn observe(&mut self, duration: Duration, bounds: &[f64]) {
        let value = duration.as_secs_f64();
        if let Some(index) = bounds.iter().position(|bound| value <= *bound) {
            self.bucket_counts[index] = self.bucket_counts[index].saturating_add(1);
        }
        self.count = self.count.saturating_add(1);
        self.sum_nanoseconds = self.sum_nanoseconds.saturating_add(duration.as_nanos());
        self.min_seconds = Some(self.min_seconds.map_or(value, |current| current.min(value)));
        self.max_seconds = Some(self.max_seconds.map_or(value, |current| current.max(value)));
    }

    fn cumulative_counts(&self) -> Vec<u64> {
        let mut total = 0_u64;
        self.bucket_counts
            .iter()
            .map(|count| {
                total = total.saturating_add(*count);
                total
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
enum MetricValue {
    Counter(u64),
    Gauge(f64),
    Histogram(HistogramValue),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

impl MetricKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

impl MetricValue {
    fn kind_name(&self) -> &'static str {
        match self {
            Self::Counter(_) => "counter",
            Self::Gauge(_) => "gauge",
            Self::Histogram(_) => "histogram",
        }
    }
}

#[derive(Debug, Clone)]
struct HealthValue {
    status: HealthStatus,
    ready: bool,
    required: bool,
    detail: Option<String>,
}

#[derive(Debug)]
struct RegistryState {
    metrics: BTreeMap<MetricKey, MetricValue>,
    metric_kinds: BTreeMap<String, MetricKind>,
    series_per_metric: BTreeMap<String, usize>,
    health: BTreeMap<HealthComponent, HealthValue>,
    recent_errors: VecDeque<RecentError>,
    next_error_sequence: u64,
    dropped_series: u64,
    saturated_counters: u64,
    total_errors: u64,
    evicted_errors: u64,
}

impl Default for RegistryState {
    fn default() -> Self {
        Self {
            metrics: BTreeMap::new(),
            metric_kinds: BTreeMap::new(),
            series_per_metric: BTreeMap::new(),
            health: BTreeMap::new(),
            recent_errors: VecDeque::new(),
            next_error_sequence: 1,
            dropped_series: 0,
            saturated_counters: 0,
            total_errors: 0,
            evicted_errors: 0,
        }
    }
}

pub struct TelemetryRegistry {
    limits: TelemetryLimits,
    correlation_prefix: String,
    next_correlation_sequence: AtomicU64,
    state: RwLock<RegistryState>,
}

impl Default for TelemetryRegistry {
    fn default() -> Self {
        Self::new(TelemetryLimits::default()).expect("default telemetry limits are valid")
    }
}

impl TelemetryRegistry {
    pub fn new(limits: TelemetryLimits) -> Result<Self, TelemetryError> {
        validate_limits(&limits)?;
        let prefix = limits
            .correlation_prefix
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        CorrelationId::parse(prefix.clone())?;
        Ok(Self {
            limits,
            correlation_prefix: prefix,
            next_correlation_sequence: AtomicU64::new(1),
            state: RwLock::new(RegistryState::default()),
        })
    }

    pub fn next_correlation_id(&self) -> CorrelationId {
        let sequence = self
            .next_correlation_sequence
            .fetch_add(1, Ordering::Relaxed);
        CorrelationId(format!("{}-{sequence:016x}", self.correlation_prefix))
    }

    pub fn increment_counter(
        &self,
        name: &str,
        labels: &[(&str, &str)],
        amount: u64,
    ) -> Result<RecordResult, TelemetryError> {
        let key = self.metric_key(name, labels)?;
        let mut state = self.state.write();
        ensure_metric_kind(&state, &key.name, MetricKind::Counter)?;
        if let Some(value) = state.metrics.get_mut(&key) {
            let saturated = match value {
                MetricValue::Counter(current) => {
                    let next = current.saturating_add(amount);
                    let saturated =
                        next == u64::MAX && *current != u64::MAX && amount > u64::MAX - *current;
                    *current = next;
                    saturated
                }
                other => return Err(kind_conflict(&key.name, "counter", other.kind_name())),
            };
            if saturated {
                state.saturated_counters = state.saturated_counters.saturating_add(1);
            }
            return Ok(RecordResult::recorded());
        }
        if !self.reserve_series(&mut state, &key.name, MetricKind::Counter) {
            return Ok(RecordResult::dropped());
        }
        state.metrics.insert(key, MetricValue::Counter(amount));
        Ok(RecordResult::recorded())
    }

    pub fn set_gauge(
        &self,
        name: &str,
        labels: &[(&str, &str)],
        value: f64,
    ) -> Result<RecordResult, TelemetryError> {
        if !value.is_finite() {
            return Err(TelemetryError::NonFiniteGauge);
        }
        let key = self.metric_key(name, labels)?;
        let mut state = self.state.write();
        ensure_metric_kind(&state, &key.name, MetricKind::Gauge)?;
        if let Some(metric) = state.metrics.get_mut(&key) {
            return match metric {
                MetricValue::Gauge(current) => {
                    *current = value;
                    Ok(RecordResult::recorded())
                }
                other => Err(kind_conflict(&key.name, "gauge", other.kind_name())),
            };
        }
        if !self.reserve_series(&mut state, &key.name, MetricKind::Gauge) {
            return Ok(RecordResult::dropped());
        }
        state.metrics.insert(key, MetricValue::Gauge(value));
        Ok(RecordResult::recorded())
    }

    pub fn observe_latency(
        &self,
        name: &str,
        labels: &[(&str, &str)],
        duration: Duration,
    ) -> Result<RecordResult, TelemetryError> {
        let key = self.metric_key(name, labels)?;
        let mut state = self.state.write();
        ensure_metric_kind(&state, &key.name, MetricKind::Histogram)?;
        if let Some(metric) = state.metrics.get_mut(&key) {
            return match metric {
                MetricValue::Histogram(histogram) => {
                    histogram.observe(duration, &self.limits.histogram_bounds_seconds);
                    Ok(RecordResult::recorded())
                }
                other => Err(kind_conflict(&key.name, "histogram", other.kind_name())),
            };
        }
        if !self.reserve_series(&mut state, &key.name, MetricKind::Histogram) {
            return Ok(RecordResult::dropped());
        }
        let mut histogram = HistogramValue::new(self.limits.histogram_bounds_seconds.len());
        histogram.observe(duration, &self.limits.histogram_bounds_seconds);
        state.metrics.insert(key, MetricValue::Histogram(histogram));
        Ok(RecordResult::recorded())
    }

    pub fn record_operation(
        &self,
        record: OperationRecord<'_>,
    ) -> Result<RecordResult, TelemetryError> {
        let operation = normalize_operation(record.operation, self.limits.max_operation_bytes);
        let labels = [
            ("subsystem", record.subsystem.as_str()),
            ("operation", operation.as_str()),
            ("status", record.status.as_str()),
        ];
        let mut result = self.increment_counter("kiralm_operations_total", &labels, 1)?;
        if let Some(duration) = record.duration {
            result.merge(self.observe_latency(
                "kiralm_operation_duration_seconds",
                &labels[..2],
                duration,
            )?);
        }
        if record.status.is_error() {
            self.push_error(
                record.subsystem,
                &operation,
                record.correlation_id,
                record.error.unwrap_or("operation failed"),
            );
        }
        Ok(result)
    }

    pub fn record_request(
        &self,
        operation: &str,
        status: OperationStatus,
        duration: Option<Duration>,
        correlation_id: Option<&CorrelationId>,
        error: Option<&str>,
    ) -> Result<RecordResult, TelemetryError> {
        self.record_subsystem(
            TelemetrySubsystem::Request,
            operation,
            status,
            duration,
            correlation_id,
            error,
        )
    }

    pub fn record_engine(
        &self,
        operation: &str,
        status: OperationStatus,
        duration: Option<Duration>,
        correlation_id: Option<&CorrelationId>,
        error: Option<&str>,
    ) -> Result<RecordResult, TelemetryError> {
        self.record_subsystem(
            TelemetrySubsystem::Engine,
            operation,
            status,
            duration,
            correlation_id,
            error,
        )
    }

    pub fn record_cache(
        &self,
        operation: &str,
        status: OperationStatus,
        duration: Option<Duration>,
        correlation_id: Option<&CorrelationId>,
        error: Option<&str>,
    ) -> Result<RecordResult, TelemetryError> {
        self.record_subsystem(
            TelemetrySubsystem::Cache,
            operation,
            status,
            duration,
            correlation_id,
            error,
        )
    }

    pub fn record_knowledge(
        &self,
        operation: &str,
        status: OperationStatus,
        duration: Option<Duration>,
        correlation_id: Option<&CorrelationId>,
        error: Option<&str>,
    ) -> Result<RecordResult, TelemetryError> {
        self.record_subsystem(
            TelemetrySubsystem::Knowledge,
            operation,
            status,
            duration,
            correlation_id,
            error,
        )
    }

    pub fn record_evaluation(
        &self,
        operation: &str,
        status: OperationStatus,
        duration: Option<Duration>,
        correlation_id: Option<&CorrelationId>,
        error: Option<&str>,
    ) -> Result<RecordResult, TelemetryError> {
        self.record_subsystem(
            TelemetrySubsystem::Evaluation,
            operation,
            status,
            duration,
            correlation_id,
            error,
        )
    }

    pub fn record_attachment(
        &self,
        operation: &str,
        status: OperationStatus,
        duration: Option<Duration>,
        correlation_id: Option<&CorrelationId>,
        error: Option<&str>,
    ) -> Result<RecordResult, TelemetryError> {
        self.record_subsystem(
            TelemetrySubsystem::Attachment,
            operation,
            status,
            duration,
            correlation_id,
            error,
        )
    }

    pub fn set_health(&self, input: HealthInput<'_>) {
        let detail = input
            .detail
            .map(|detail| redact_bounded(detail, self.limits.max_error_message_bytes));
        self.state.write().health.insert(
            input.component,
            HealthValue {
                status: input.status,
                ready: input.ready,
                required: input.required,
                detail,
            },
        );
    }

    pub fn clear_health(&self, component: HealthComponent) {
        self.state.write().health.remove(&component);
    }

    pub fn health_summary(&self) -> HealthSummary {
        health_snapshot(&self.state.read().health)
    }

    pub fn snapshot(&self) -> TelemetrySnapshot {
        let state = self.state.read();
        let mut counters = Vec::new();
        let mut gauges = Vec::new();
        let mut histograms = Vec::new();
        for (key, value) in &state.metrics {
            match value {
                MetricValue::Counter(value) => counters.push(CounterSnapshot {
                    name: key.name.clone(),
                    labels: key.labels.clone(),
                    value: *value,
                }),
                MetricValue::Gauge(value) => gauges.push(GaugeSnapshot {
                    name: key.name.clone(),
                    labels: key.labels.clone(),
                    value: *value,
                }),
                MetricValue::Histogram(value) => histograms.push(HistogramSnapshot {
                    name: key.name.clone(),
                    labels: key.labels.clone(),
                    bounds_seconds: self.limits.histogram_bounds_seconds.clone(),
                    cumulative_counts: value.cumulative_counts(),
                    count: value.count,
                    sum_seconds: value.sum_nanoseconds as f64 / 1_000_000_000.0,
                    min_seconds: value.min_seconds,
                    max_seconds: value.max_seconds,
                }),
            }
        }
        TelemetrySnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            counters,
            gauges,
            histograms,
            health: health_snapshot(&state.health),
            recent_errors: state.recent_errors.iter().cloned().collect(),
            stats: TelemetryStats {
                series: state.metrics.len(),
                dropped_series: state.dropped_series,
                saturated_counters: state.saturated_counters,
                total_errors: state.total_errors,
                evicted_errors: state.evicted_errors,
            },
        }
    }

    pub fn render_prometheus(&self) -> String {
        let snapshot = self.snapshot();
        let mut output = String::new();
        for counter in &snapshot.counters {
            write_prometheus_sample(
                &mut output,
                &counter.name,
                &counter.labels,
                &counter.value.to_string(),
            );
        }
        for gauge in &snapshot.gauges {
            write_prometheus_sample(
                &mut output,
                &gauge.name,
                &gauge.labels,
                &format_number(gauge.value),
            );
        }
        for histogram in &snapshot.histograms {
            for (bound, count) in histogram
                .bounds_seconds
                .iter()
                .zip(&histogram.cumulative_counts)
            {
                let mut labels = histogram.labels.clone();
                labels.insert("le".into(), format_number(*bound));
                write_prometheus_sample(
                    &mut output,
                    &format!("{}_bucket", histogram.name),
                    &labels,
                    &count.to_string(),
                );
            }
            let mut infinite_labels = histogram.labels.clone();
            infinite_labels.insert("le".into(), "+Inf".into());
            write_prometheus_sample(
                &mut output,
                &format!("{}_bucket", histogram.name),
                &infinite_labels,
                &histogram.count.to_string(),
            );
            write_prometheus_sample(
                &mut output,
                &format!("{}_sum", histogram.name),
                &histogram.labels,
                &format_number(histogram.sum_seconds),
            );
            write_prometheus_sample(
                &mut output,
                &format!("{}_count", histogram.name),
                &histogram.labels,
                &histogram.count.to_string(),
            );
        }
        let internal = [
            (
                "kiralm_telemetry_dropped_series_total",
                snapshot.stats.dropped_series,
            ),
            (
                "kiralm_telemetry_saturated_counters_total",
                snapshot.stats.saturated_counters,
            ),
            ("kiralm_telemetry_errors_total", snapshot.stats.total_errors),
            (
                "kiralm_telemetry_evicted_errors_total",
                snapshot.stats.evicted_errors,
            ),
        ];
        for (name, value) in internal {
            write_prometheus_sample(&mut output, name, &BTreeMap::new(), &value.to_string());
        }
        for component in &snapshot.health.components {
            let labels = BTreeMap::from([(
                "component".to_string(),
                component.component.as_str().to_string(),
            )]);
            let value = match component.status {
                HealthStatus::Healthy => "1",
                HealthStatus::Degraded => "0.5",
                HealthStatus::Unhealthy => "0",
            };
            write_prometheus_sample(&mut output, "kiralm_component_health", &labels, value);
        }
        write_prometheus_sample(
            &mut output,
            "kiralm_ready",
            &BTreeMap::new(),
            if snapshot.health.ready { "1" } else { "0" },
        );
        output
    }

    pub fn reset(&self) {
        *self.state.write() = RegistryState::default();
        self.next_correlation_sequence.store(1, Ordering::Relaxed);
    }

    fn record_subsystem(
        &self,
        subsystem: TelemetrySubsystem,
        operation: &str,
        status: OperationStatus,
        duration: Option<Duration>,
        correlation_id: Option<&CorrelationId>,
        error: Option<&str>,
    ) -> Result<RecordResult, TelemetryError> {
        let mut record = OperationRecord::new(subsystem, operation, status);
        record.duration = duration;
        record.correlation_id = correlation_id;
        record.error = error;
        self.record_operation(record)
    }

    fn metric_key(&self, name: &str, labels: &[(&str, &str)]) -> Result<MetricKey, TelemetryError> {
        validate_identifier(name, self.limits.max_metric_name_len).map_err(|_| {
            TelemetryError::InvalidMetricName(bounded_text(name, self.limits.max_metric_name_len))
        })?;
        if is_internal_metric(name) {
            return Err(TelemetryError::InvalidMetricName(name.to_string()));
        }
        if labels.len() > self.limits.max_labels_per_series {
            return Err(TelemetryError::TooManyLabels);
        }
        let mut normalized = BTreeMap::new();
        for (label_name, label_value) in labels {
            validate_identifier(label_name, self.limits.max_label_name_len).map_err(|_| {
                TelemetryError::InvalidLabelName(bounded_text(
                    label_name,
                    self.limits.max_label_name_len,
                ))
            })?;
            let value = if is_sensitive_key(label_name) {
                "[REDACTED]".to_string()
            } else {
                redact_bounded(label_value, self.limits.max_label_value_bytes)
            };
            normalized.insert((*label_name).to_string(), value);
        }
        Ok(MetricKey {
            name: name.to_string(),
            labels: normalized,
        })
    }

    fn reserve_series(
        &self,
        state: &mut RegistryState,
        metric_name: &str,
        kind: MetricKind,
    ) -> bool {
        let metric_count = state
            .series_per_metric
            .get(metric_name)
            .copied()
            .unwrap_or(0);
        if state.metrics.len() >= self.limits.max_series
            || metric_count >= self.limits.max_series_per_metric
        {
            state.dropped_series = state.dropped_series.saturating_add(1);
            return false;
        }
        state
            .series_per_metric
            .insert(metric_name.to_string(), metric_count + 1);
        state.metric_kinds.insert(metric_name.to_string(), kind);
        true
    }

    fn push_error(
        &self,
        subsystem: TelemetrySubsystem,
        operation: &str,
        correlation_id: Option<&CorrelationId>,
        message: &str,
    ) {
        let mut state = self.state.write();
        let error = RecentError {
            sequence: state.next_error_sequence,
            subsystem,
            operation: operation.to_string(),
            correlation_id: correlation_id.cloned(),
            message: redact_bounded(message, self.limits.max_error_message_bytes),
        };
        state.next_error_sequence = state.next_error_sequence.saturating_add(1);
        state.total_errors = state.total_errors.saturating_add(1);
        if self.limits.max_recent_errors == 0 {
            state.evicted_errors = state.evicted_errors.saturating_add(1);
            return;
        }
        if state.recent_errors.len() == self.limits.max_recent_errors {
            state.recent_errors.pop_front();
            state.evicted_errors = state.evicted_errors.saturating_add(1);
        }
        state.recent_errors.push_back(error);
    }
}

fn validate_limits(limits: &TelemetryLimits) -> Result<(), TelemetryError> {
    if limits.max_series == 0
        || limits.max_series_per_metric == 0
        || limits.max_series_per_metric > limits.max_series
        || limits.max_labels_per_series == 0
        || limits.max_metric_name_len == 0
        || limits.max_label_name_len == 0
        || limits.max_label_value_bytes == 0
        || limits.max_operation_bytes == 0
        || limits.max_error_message_bytes == 0
    {
        return Err(TelemetryError::InvalidLimits(
            "cardinality and text bounds must be positive and consistent".into(),
        ));
    }
    if limits.max_series > 100_000
        || limits.max_series_per_metric > 10_000
        || limits.max_labels_per_series > 32
        || limits.max_metric_name_len > 256
        || limits.max_label_name_len > 128
        || limits.max_label_value_bytes > 4_096
        || limits.max_operation_bytes > 256
        || limits.max_recent_errors > 4_096
        || limits.max_error_message_bytes > 65_536
        || limits.histogram_bounds_seconds.len() > 64
    {
        return Err(TelemetryError::InvalidLimits(
            "configured limits exceed telemetry safety ceilings".into(),
        ));
    }
    if limits.histogram_bounds_seconds.is_empty() {
        return Err(TelemetryError::InvalidLimits(
            "at least one histogram bound is required".into(),
        ));
    }
    let mut previous = 0.0;
    for bound in &limits.histogram_bounds_seconds {
        if !bound.is_finite() || *bound <= previous {
            return Err(TelemetryError::InvalidLimits(
                "histogram bounds must be finite, positive, and increasing".into(),
            ));
        }
        previous = *bound;
    }
    if let Some(prefix) = &limits.correlation_prefix {
        if prefix.len() > 48 {
            return Err(TelemetryError::InvalidLimits(
                "correlation prefix exceeds 48 bytes".into(),
            ));
        }
        CorrelationId::parse(prefix.clone())?;
    }
    Ok(())
}

fn validate_identifier(value: &str, max_len: usize) -> Result<(), ()> {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return Err(());
    };
    if value.len() > max_len || !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(());
    }
    if bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
        Ok(())
    } else {
        Err(())
    }
}

fn is_internal_metric(name: &str) -> bool {
    matches!(
        name,
        "kiralm_telemetry_dropped_series_total"
            | "kiralm_telemetry_saturated_counters_total"
            | "kiralm_telemetry_errors_total"
            | "kiralm_telemetry_evicted_errors_total"
            | "kiralm_component_health"
            | "kiralm_ready"
    )
}

fn kind_conflict(name: &str, expected: &str, actual: &str) -> TelemetryError {
    TelemetryError::MetricKindConflict(format!(
        "{name} is {actual}, cannot record it as {expected}"
    ))
}

fn ensure_metric_kind(
    state: &RegistryState,
    name: &str,
    expected: MetricKind,
) -> Result<(), TelemetryError> {
    if let Some(actual) = state.metric_kinds.get(name) {
        if *actual != expected {
            return Err(kind_conflict(name, expected.as_str(), actual.as_str()));
        }
    }
    Ok(())
}

fn normalize_operation(value: &str, max_bytes: usize) -> String {
    let normalized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || "_./:-".contains(character) {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let normalized = normalized.trim_matches('_');
    if normalized.is_empty() {
        "unknown".into()
    } else {
        bounded_text(normalized, max_bytes)
    }
}

fn health_snapshot(health: &BTreeMap<HealthComponent, HealthValue>) -> HealthSummary {
    let mut status = HealthStatus::Healthy;
    let mut ready = true;
    let components = health
        .iter()
        .map(|(component, value)| {
            status = match (status, value.status) {
                (HealthStatus::Unhealthy, _) | (_, HealthStatus::Unhealthy) => {
                    HealthStatus::Unhealthy
                }
                (HealthStatus::Degraded, _) | (_, HealthStatus::Degraded) => HealthStatus::Degraded,
                _ => HealthStatus::Healthy,
            };
            if value.required && !value.ready {
                ready = false;
            }
            HealthComponentSnapshot {
                component: *component,
                status: value.status,
                ready: value.ready,
                required: value.required,
                detail: value.detail.clone(),
            }
        })
        .collect();
    HealthSummary {
        status,
        ready,
        components,
    }
}

pub fn redact_sensitive(value: &str) -> String {
    let mut redacted = redact_marker(value, "bearer", false);
    for key in [
        "authorization",
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
        "token",
        "password",
        "passwd",
        "secret",
        "cookie",
        "credential",
    ] {
        redacted = redact_marker(&redacted, key, true);
    }
    redacted
}

fn redact_marker(value: &str, marker: &str, needs_separator: bool) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while let Some(relative) = value[cursor..].to_ascii_lowercase().find(marker) {
        let start = cursor + relative;
        let end = start + marker.len();
        if !has_marker_boundaries(value, start, end) {
            output.push_str(&value[cursor..end]);
            cursor = end;
            continue;
        }
        let Some((secret_start, secret_end)) = secret_span(value, end, needs_separator) else {
            output.push_str(&value[cursor..end]);
            cursor = end;
            continue;
        };
        output.push_str(&value[cursor..secret_start]);
        output.push_str("[REDACTED]");
        cursor = secret_end;
    }
    output.push_str(&value[cursor..]);
    output
}

fn has_marker_boundaries(value: &str, start: usize, end: usize) -> bool {
    let before = value[..start].chars().next_back();
    let after = value[end..].chars().next();
    !before.is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
        && !after.is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn secret_span(value: &str, marker_end: usize, needs_separator: bool) -> Option<(usize, usize)> {
    let bytes = value.as_bytes();
    let mut cursor = marker_end;
    while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t' | b'\'' | b'"') {
        cursor += 1;
    }
    if needs_separator {
        if cursor >= bytes.len() || !matches!(bytes[cursor], b':' | b'=') {
            return None;
        }
        cursor += 1;
        while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
            cursor += 1;
        }
    }
    let quote = if cursor < bytes.len() && matches!(bytes[cursor], b'\'' | b'"') {
        let quote = Some(bytes[cursor]);
        cursor += 1;
        quote
    } else {
        None
    };
    let secret_start = cursor;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        let ends_secret = quote.map_or(
            matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b',' | b';' | b'&'),
            |quote| byte == quote,
        );
        if ends_secret {
            break;
        }
        cursor += 1;
    }
    (cursor > secret_start).then_some((secret_start, cursor))
}

fn is_sensitive_key(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "authorization",
        "api_key",
        "apikey",
        "token",
        "password",
        "passwd",
        "secret",
        "cookie",
        "credential",
    ]
    .iter()
    .any(|sensitive| value.contains(sensitive))
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    let mut sanitized = String::with_capacity(value.len().min(max_bytes));
    let mut truncated = false;
    for character in value.chars() {
        let character = if character.is_control() && !matches!(character, '\n' | '\t') {
            ' '
        } else {
            character
        };
        if sanitized.len() + character.len_utf8() > max_bytes {
            truncated = true;
            break;
        }
        sanitized.push(character);
    }
    if !truncated {
        return sanitized;
    }
    if max_bytes <= 3 {
        return ".".repeat(max_bytes);
    }
    while sanitized.len() > max_bytes - 3 {
        sanitized.pop();
    }
    sanitized.push_str("...");
    sanitized
}

fn redact_bounded(value: &str, max_bytes: usize) -> String {
    let scan_limit = max_bytes.saturating_mul(2).max(max_bytes);
    let candidate = bounded_text(value, scan_limit);
    bounded_text(&redact_sensitive(&candidate), max_bytes)
}

fn format_number(value: f64) -> String {
    if value == 0.0 {
        "0".into()
    } else {
        value.to_string()
    }
}

fn write_prometheus_sample(
    output: &mut String,
    name: &str,
    labels: &BTreeMap<String, String>,
    value: &str,
) {
    output.push_str(name);
    if !labels.is_empty() {
        output.push('{');
        for (index, (label, value)) in labels.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            output.push_str(label);
            output.push_str("=\"");
            for character in value.chars() {
                match character {
                    '\\' => output.push_str("\\\\"),
                    '"' => output.push_str("\\\""),
                    '\n' => output.push_str("\\n"),
                    _ => output.push(character),
                }
            }
            output.push('"');
        }
        output.push('}');
    }
    output.push(' ');
    output.push_str(value);
    output.push('\n');
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    fn test_registry() -> TelemetryRegistry {
        TelemetryRegistry::new(TelemetryLimits {
            correlation_prefix: Some("test".into()),
            histogram_bounds_seconds: vec![0.1, 0.5, 1.0],
            ..TelemetryLimits::default()
        })
        .unwrap()
    }

    #[test]
    fn records_counters_gauges_and_histograms() {
        let telemetry = test_registry();
        telemetry
            .increment_counter("requests_total", &[("status", "ok")], 2)
            .unwrap();
        telemetry
            .increment_counter("requests_total", &[("status", "ok")], 3)
            .unwrap();
        telemetry.set_gauge("queue_depth", &[], 4.5).unwrap();
        for milliseconds in [50, 250, 750, 2_000] {
            telemetry
                .observe_latency(
                    "request_seconds",
                    &[("route", "chat")],
                    Duration::from_millis(milliseconds),
                )
                .unwrap();
        }

        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.counters[0].value, 5);
        assert_eq!(snapshot.gauges[0].value, 4.5);
        assert_eq!(snapshot.histograms[0].cumulative_counts, vec![1, 2, 3]);
        assert_eq!(snapshot.histograms[0].count, 4);
        assert_eq!(snapshot.histograms[0].sum_seconds, 3.05);
        assert_eq!(snapshot.histograms[0].min_seconds, Some(0.05));
        assert_eq!(snapshot.histograms[0].max_seconds, Some(2.0));
    }

    #[test]
    fn serializes_and_renders_in_stable_order() {
        let telemetry = test_registry();
        telemetry
            .set_gauge("z_gauge", &[("zone", "b")], 2.0)
            .unwrap();
        telemetry.increment_counter("a_total", &[], 1).unwrap();
        telemetry
            .observe_latency(
                "latency_seconds",
                &[("route", "a\"b")],
                Duration::from_millis(50),
            )
            .unwrap();

        let first = telemetry.snapshot().to_stable_json().unwrap();
        let second = telemetry.snapshot().to_stable_json().unwrap();
        assert_eq!(first, second);
        let prometheus = telemetry.render_prometheus();
        assert!(prometheus.starts_with("a_total 1\nz_gauge{zone=\"b\"} 2\n"));
        assert!(prometheus.contains("latency_seconds_bucket{le=\"0.1\",route=\"a\\\"b\"} 1\n"));
        assert!(prometheus.contains("latency_seconds_bucket{le=\"+Inf\",route=\"a\\\"b\"} 1\n"));
        assert!(prometheus.ends_with("kiralm_ready 1\n"));
    }

    #[test]
    fn bounds_global_and_per_metric_cardinality() {
        let telemetry = TelemetryRegistry::new(TelemetryLimits {
            max_series: 3,
            max_series_per_metric: 2,
            correlation_prefix: Some("test".into()),
            ..TelemetryLimits::default()
        })
        .unwrap();
        assert_eq!(
            telemetry
                .increment_counter("items_total", &[("id", "one")], 1)
                .unwrap(),
            RecordResult::recorded()
        );
        telemetry
            .increment_counter("items_total", &[("id", "two")], 1)
            .unwrap();
        assert_eq!(
            telemetry
                .increment_counter("items_total", &[("id", "three")], 1)
                .unwrap(),
            RecordResult::dropped()
        );
        telemetry.set_gauge("first", &[], 1.0).unwrap();
        assert_eq!(
            telemetry.set_gauge("second", &[], 2.0).unwrap(),
            RecordResult::dropped()
        );

        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.stats.series, 3);
        assert_eq!(snapshot.stats.dropped_series, 2);
    }

    #[test]
    fn bounds_and_redacts_labels_and_errors() {
        let telemetry = TelemetryRegistry::new(TelemetryLimits {
            max_label_value_bytes: 16,
            max_error_message_bytes: 96,
            max_recent_errors: 2,
            correlation_prefix: Some("test".into()),
            ..TelemetryLimits::default()
        })
        .unwrap();
        telemetry
            .increment_counter(
                "requests_total",
                &[
                    ("api_key", "super-secret"),
                    ("path", "a-very-long-label-value"),
                ],
                1,
            )
            .unwrap();
        let correlation = telemetry.next_correlation_id();
        for error in [
            "authorization: Bearer alpha token=beta password='gamma'",
            "cookie=session-id api_key=delta",
            "credential=epsilon refresh_token=zeta",
        ] {
            telemetry
                .record_request(
                    "chat/completions",
                    OperationStatus::Error,
                    None,
                    Some(&correlation),
                    Some(error),
                )
                .unwrap();
        }

        let snapshot = telemetry.snapshot();
        let labels = &snapshot
            .counters
            .iter()
            .find(|counter| counter.name == "requests_total")
            .unwrap()
            .labels;
        assert_eq!(labels["api_key"], "[REDACTED]");
        assert_eq!(labels["path"], "a-very-long-l...");
        assert_eq!(snapshot.recent_errors.len(), 2);
        assert_eq!(snapshot.stats.total_errors, 3);
        assert_eq!(snapshot.stats.evicted_errors, 1);
        let json = snapshot.to_stable_json().unwrap();
        for secret in [
            "alpha",
            "beta",
            "gamma",
            "session-id",
            "delta",
            "epsilon",
            "zeta",
        ] {
            assert!(!json.contains(secret), "secret leaked: {secret}");
        }
        assert!(json.contains("[REDACTED]"));
    }

    #[test]
    fn operation_helpers_cover_every_subsystem() {
        let telemetry = test_registry();
        let duration = Some(Duration::from_millis(10));
        telemetry
            .record_request("chat", OperationStatus::Success, duration, None, None)
            .unwrap();
        telemetry
            .record_engine("load", OperationStatus::Success, duration, None, None)
            .unwrap();
        telemetry
            .record_cache("prune", OperationStatus::Success, duration, None, None)
            .unwrap();
        telemetry
            .record_knowledge("index", OperationStatus::Success, duration, None, None)
            .unwrap();
        telemetry
            .record_evaluation("run", OperationStatus::Success, duration, None, None)
            .unwrap();
        telemetry
            .record_attachment("store", OperationStatus::Success, duration, None, None)
            .unwrap();

        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.counters.len(), 6);
        assert_eq!(snapshot.histograms.len(), 6);
        assert_eq!(snapshot.stats.series, 12);
    }

    #[test]
    fn aggregates_health_and_readiness_independently() {
        let telemetry = test_registry();
        telemetry.set_health(HealthInput {
            component: HealthComponent::Daemon,
            status: HealthStatus::Healthy,
            ready: true,
            required: true,
            detail: None,
        });
        telemetry.set_health(HealthInput {
            component: HealthComponent::Knowledge,
            status: HealthStatus::Degraded,
            ready: false,
            required: false,
            detail: Some("token=hidden fallback active"),
        });
        let summary = telemetry.health_summary();
        assert_eq!(summary.status, HealthStatus::Degraded);
        assert!(summary.ready);
        assert_eq!(
            summary.components[1].detail.as_deref(),
            Some("token=[REDACTED] fallback active")
        );

        telemetry.set_health(HealthInput {
            component: HealthComponent::Storage,
            status: HealthStatus::Unhealthy,
            ready: false,
            required: true,
            detail: None,
        });
        let summary = telemetry.health_summary();
        assert_eq!(summary.status, HealthStatus::Unhealthy);
        assert!(!summary.ready);
        telemetry.clear_health(HealthComponent::Storage);
        assert!(telemetry.health_summary().ready);
    }

    #[test]
    fn records_concurrent_updates_without_loss() {
        let telemetry = Arc::new(test_registry());
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let telemetry = telemetry.clone();
                thread::spawn(move || {
                    for _ in 0..1_000 {
                        telemetry
                            .increment_counter("work_total", &[("kind", "shared")], 1)
                            .unwrap();
                        telemetry
                            .observe_latency("work_seconds", &[], Duration::from_millis(5))
                            .unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }

        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.counters[0].value, 16_000);
        assert_eq!(snapshot.histograms[0].count, 16_000);
        assert_eq!(
            snapshot.histograms[0].cumulative_counts,
            vec![16_000, 16_000, 16_000]
        );
    }

    #[test]
    fn reset_restores_deterministic_state_and_ids() {
        let telemetry = test_registry();
        assert_eq!(
            telemetry.next_correlation_id().as_str(),
            "test-0000000000000001"
        );
        telemetry.increment_counter("work_total", &[], 2).unwrap();
        telemetry
            .record_engine(
                "load",
                OperationStatus::Error,
                None,
                None,
                Some("token=secret"),
            )
            .unwrap();
        telemetry.set_health(HealthInput {
            component: HealthComponent::Engine,
            status: HealthStatus::Unhealthy,
            ready: false,
            required: true,
            detail: None,
        });
        telemetry.reset();

        assert_eq!(telemetry.snapshot(), test_registry().snapshot());
        assert_eq!(
            telemetry.next_correlation_id().as_str(),
            "test-0000000000000001"
        );
    }

    #[test]
    fn rejects_invalid_metric_shapes_and_kind_conflicts() {
        let telemetry = test_registry();
        assert!(matches!(
            telemetry.increment_counter("9bad", &[], 1),
            Err(TelemetryError::InvalidMetricName(_))
        ));
        assert!(matches!(
            telemetry.set_gauge("kiralm_ready", &[], 1.0),
            Err(TelemetryError::InvalidMetricName(_))
        ));
        assert!(matches!(
            telemetry.increment_counter("valid", &[("bad-label", "x")], 1),
            Err(TelemetryError::InvalidLabelName(_))
        ));
        let labels = [("a", "1"), ("b", "2"), ("c", "3")];
        let limited = TelemetryRegistry::new(TelemetryLimits {
            max_labels_per_series: 2,
            correlation_prefix: Some("test".into()),
            ..TelemetryLimits::default()
        })
        .unwrap();
        assert_eq!(
            limited.increment_counter("valid", &labels, 1),
            Err(TelemetryError::TooManyLabels)
        );
        telemetry.increment_counter("same_name", &[], 1).unwrap();
        assert!(matches!(
            telemetry.set_gauge("same_name", &[], 1.0),
            Err(TelemetryError::MetricKindConflict(_))
        ));
        assert!(matches!(
            telemetry.set_gauge("same_name", &[("kind", "other")], 1.0),
            Err(TelemetryError::MetricKindConflict(_))
        ));
        assert_eq!(
            CorrelationId::parse("unsafe correlation"),
            Err(TelemetryError::InvalidCorrelationId)
        );
    }

    #[test]
    fn truncates_utf8_without_splitting_code_points() {
        let telemetry = TelemetryRegistry::new(TelemetryLimits {
            max_label_value_bytes: 8,
            correlation_prefix: Some("test".into()),
            ..TelemetryLimits::default()
        })
        .unwrap();
        telemetry
            .set_gauge("value", &[("label", "éééééé")], 1.0)
            .unwrap();
        assert_eq!(telemetry.snapshot().gauges[0].labels["label"], "éé...");
    }

    #[test]
    fn validates_limits_and_tracks_counter_saturation() {
        assert!(matches!(
            TelemetryRegistry::new(TelemetryLimits {
                histogram_bounds_seconds: vec![1.0, 0.5],
                ..TelemetryLimits::default()
            }),
            Err(TelemetryError::InvalidLimits(_))
        ));
        let telemetry = test_registry();
        telemetry
            .increment_counter("bytes_total", &[], u64::MAX - 1)
            .unwrap();
        telemetry.increment_counter("bytes_total", &[], 10).unwrap();
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.counters[0].value, u64::MAX);
        assert_eq!(snapshot.stats.saturated_counters, 1);
    }
}
