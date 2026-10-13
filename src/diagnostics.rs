//! Bounded, optional diagnostic telemetry. Signed durable audit evidence is independent.
use crate::telemetry::{Correlation, TraceContext};
use anyhow::{Context as _, Result, ensure};
use opentelemetry::{
    Context, KeyValue,
    metrics::MeterProvider,
    trace::{
        SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState, Tracer,
        TracerProvider,
    },
};
use opentelemetry_sdk::trace::{IdGenerator, RandomIdGenerator};
use opentelemetry_sdk::{
    Resource,
    error::{OTelSdkError, OTelSdkResult},
    metrics::{
        PeriodicReader, SdkMeterProvider, Temporality, data::ResourceMetrics,
        exporter::PushMetricExporter,
    },
    trace::{
        BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracer, SdkTracerProvider, SpanData,
        SpanExporter, SpanLimits,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    io::Read,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::prelude::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiagnosticsConfig {
    /// Explicit HTTP collector base, e.g. http://127.0.0.1:4318. No ambient endpoint.
    pub otlp_http_endpoint: Option<String>,
    pub queue_size: usize,
    pub batch_size: usize,
    pub export_interval_ms: u64,
    pub export_timeout_ms: u64,
    pub shutdown_timeout_ms: u64,
}
impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            otlp_http_endpoint: None,
            queue_size: 512,
            batch_size: 64,
            export_interval_ms: 250,
            export_timeout_ms: 1000,
            shutdown_timeout_ms: 3000,
        }
    }
}
impl DiagnosticsConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=8192).contains(&self.queue_size)
                && (1..=128).contains(&self.batch_size)
                && self.batch_size <= self.queue_size,
            "diagnostic queue or batch bound invalid"
        );
        ensure!(
            (50..=60000).contains(&self.export_interval_ms)
                && (50..=5000).contains(&self.export_timeout_ms)
                && (100..=10000).contains(&self.shutdown_timeout_ms),
            "diagnostic timeout bound invalid"
        );
        if let Some(endpoint) = &self.otlp_http_endpoint {
            approved_endpoint(endpoint)?;
        }
        Ok(())
    }
}
fn approved_endpoint(endpoint: &str) -> Result<reqwest::Url> {
    ensure!(endpoint.len() <= 256, "diagnostic endpoint exceeds bound");
    let u = reqwest::Url::parse(endpoint)?;
    ensure!(
        u.scheme() == "http"
            && u.host_str()
                .and_then(|host| host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .ok())
                .is_some_and(|ip| ip.is_loopback()),
        "diagnostic collector must be explicit literal loopback HTTP"
    );
    ensure!(
        u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
            && matches!(u.path(), "" | "/"),
        "diagnostic collector has credentials or unexpected path"
    );
    ensure!(
        u.port_or_known_default().is_some_and(|p| p > 0),
        "diagnostic collector port invalid"
    );
    Ok(u)
}

#[derive(Clone, Copy, Debug)]
pub enum Stage {
    Task,
    Message,
    Provider,
    Tool,
    Hop,
    Scheduling,
    State,
    ProviderQueue,
    Lease,
    Cleanup,
    Reconcile,
    Raft,
}
impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Message => "message",
            Self::Provider => "provider",
            Self::Tool => "tool",
            Self::Hop => "hop",
            Self::Scheduling => "scheduling",
            Self::State => "state",
            Self::ProviderQueue => "provider_queue",
            Self::Lease => "lease",
            Self::Cleanup => "cleanup",
            Self::Reconcile => "reconcile",
            Self::Raft => "raft",
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Outcome {
    Success,
    Denied,
    Unknown,
    Failure,
    Cancelled,
    Error,
}
impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Denied => "denied",
            Self::Unknown => "unknown",
            Self::Failure | Self::Error => "failure",
            Self::Cancelled => "cancelled",
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Gauge {
    ActiveWorkers,
    IngressBulkAvailable,
    IngressControlAvailable,
    RaftLeader,
    RaftAppliedIndex,
    RaftTerm,
    RaftLogIndex,
    RaftQuorumAckAgeMillis,
    TransportConnections,
    TransportRttMicros,
    TransportLostPackets,
    TransportSentPackets,
    TransportCongestionWindowBytes,
    MoneySpentMicrousd,
    MoneyHeldMicrousd,
}
impl Gauge {
    fn as_str(self) -> &'static str {
        match self {
            Self::ActiveWorkers => "active_workers",
            Self::IngressBulkAvailable => "ingress_bulk_available",
            Self::IngressControlAvailable => "ingress_control_available",
            Self::RaftLeader => "raft_leader",
            Self::RaftAppliedIndex => "raft_applied_index",
            Self::RaftTerm => "raft_term",
            Self::RaftLogIndex => "raft_log_index",
            Self::RaftQuorumAckAgeMillis => "raft_quorum_ack_age_millis",
            Self::TransportConnections => "transport_connections",
            Self::TransportRttMicros => "transport_rtt_micros",
            Self::TransportLostPackets => "transport_lost_packets",
            Self::TransportSentPackets => "transport_sent_packets",
            Self::TransportCongestionWindowBytes => "transport_congestion_window_bytes",
            Self::MoneySpentMicrousd => "money_spent_microusd",
            Self::MoneyHeldMicrousd => "money_held_microusd",
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Counter {
    RaftApplied,
    RaftRejected,
    TransportBytesSent,
    TransportBytesReceived,
}
impl Counter {
    fn as_str(self) -> &'static str {
        match self {
            Self::RaftApplied => "raft_applied",
            Self::RaftRejected => "raft_rejected",
            Self::TransportBytesSent => "transport_bytes_sent",
            Self::TransportBytesReceived => "transport_bytes_received",
        }
    }
}
struct Instruments {
    operations: opentelemetry::metrics::Counter<u64>,
    latency: opentelemetry::metrics::Histogram<f64>,
    gauge: opentelemetry::metrics::Gauge<u64>,
    counters: opentelemetry::metrics::Counter<u64>,
    tokens: opentelemetry::metrics::Counter<u64>,
}
static METRICS: OnceLock<Instruments> = OnceLock::new();
static TRACER: OnceLock<SdkTracer> = OnceLock::new();
tokio::task_local! {static TASK_TRACE:Option<TraceContext>;}
pub async fn with_trace_context<T>(
    trace: Option<TraceContext>,
    future: impl Future<Output = T>,
) -> T {
    TASK_TRACE.scope(trace, future).await
}
pub fn current_trace_context() -> Option<TraceContext> {
    let c = tracing::Span::current().context();
    let sc = c.span().span_context().clone();
    if sc.is_valid() {
        Some(TraceContext {
            trace_id: sc.trace_id().to_string(),
            span_id: sc.span_id().to_string(),
            trace_flags: sc.trace_flags().to_u8(),
            tracestate: (!sc.trace_state().header().is_empty()).then(|| sc.trace_state().header()),
        })
    } else {
        TASK_TRACE.try_with(Clone::clone).ok().flatten()
    }
}
pub fn current_trace_headers() -> Option<(String, Option<String>)> {
    current_trace_context().map(|c| (c.traceparent(), c.tracestate))
}
pub fn attach_parent(span: &tracing::Span, parent: &TraceContext) -> Result<()> {
    let validated = TraceContext::parse(&parent.traceparent(), parent.tracestate.as_deref())?;
    if TRACER.get().is_none() {
        return Ok(());
    }
    let state = validated
        .tracestate
        .as_deref()
        .map(str::parse::<TraceState>)
        .transpose()?
        .unwrap_or_default();
    let context = SpanContext::new(
        TraceId::from_hex(&validated.trace_id)?,
        SpanId::from_hex(&validated.span_id)?,
        TraceFlags::new(validated.trace_flags),
        true,
        state,
    );
    span.set_parent(Context::new().with_remote_span_context(context))
        .map_err(|_| anyhow::anyhow!("diagnostic parent attachment unavailable"))
}
thread_local! {static ROOT_IDS:std::cell::Cell<Option<(TraceId,SpanId)>>=const {std::cell::Cell::new(None)};}
#[derive(Debug, Default)]
struct CanonicalIds {
    random: RandomIdGenerator,
}
impl IdGenerator for CanonicalIds {
    fn new_trace_id(&self) -> TraceId {
        ROOT_IDS.with(|s| {
            s.get()
                .map(|v| v.0)
                .unwrap_or_else(|| self.random.new_trace_id())
        })
    }
    fn new_span_id(&self) -> SpanId {
        ROOT_IDS.with(|s| {
            s.get()
                .map(|v| v.1)
                .unwrap_or_else(|| self.random.new_span_id())
        })
    }
}
fn correlation_attributes(c: &Correlation) -> Vec<KeyValue> {
    let mut fields = vec![KeyValue::new("deployment_id", c.deployment_id.clone())];
    for (key, value) in [
        ("run_id", &c.run_id),
        ("goal_id", &c.goal_id),
        ("task_id", &c.task_id),
        ("intent_id", &c.intent_id),
        ("agent_id", &c.agent_id),
        ("node_id", &c.node_id),
    ] {
        if let Some(value) = value
            && value.len() <= 256
        {
            fields.push(KeyValue::new(key, value.clone()));
        }
    }
    if let Some(value) = c.incarnation.and_then(|value| i64::try_from(value).ok()) {
        fields.push(KeyValue::new("incarnation", value));
    }
    if let Some(value) = c
        .execution_attempt
        .and_then(|value| i64::try_from(value).ok())
    {
        fields.push(KeyValue::new("execution_attempt", value));
    }
    fields
}
pub fn task_span(c: &Correlation) -> tracing::Span {
    let span = correlated_span(c, Stage::Task, c.execution_attempt.unwrap_or(0));
    if let (Some(tracer), Some(t)) = (TRACER.get(), c.trace_context.as_ref())
        && let (Ok(trace_id), Ok(span_id)) =
            (TraceId::from_hex(&t.trace_id), SpanId::from_hex(&t.span_id))
    {
        // Emit the canonical Task root itself; no implicit daemon/global parent.
        let mut builder = tracer.span_builder("task.root");
        builder.attributes = Some(correlation_attributes(c));
        let root = ROOT_IDS.with(|slot| {
            let old = slot.replace(Some((trace_id, span_id)));
            let root = builder.start_with_context(tracer, &Context::new());
            slot.set(old);
            root
        });
        let _ = span.set_parent(Context::new().with_span(root));
    }
    span
}
pub fn operation_span(c: &Correlation, stage: Stage, attempt: u64) -> tracing::Span {
    let span = correlated_span(c, stage, attempt);
    if let Some(t) = current_trace_context().or_else(|| c.trace_context.clone()) {
        let _ = attach_parent(&span, &t);
    }
    span
}
/// Node identities come from the authenticated, admitted inventory, never RPC metadata.
pub fn hop_span(
    parent: &TraceContext,
    sender_node: &str,
    receiver_node: &str,
    queue_wait_ms: u64,
) -> tracing::Span {
    if parent.validate().is_err()
        || sender_node.is_empty()
        || receiver_node.is_empty()
        || sender_node.len() > 128
        || receiver_node.len() > 128
    {
        return tracing::Span::none();
    }
    let span = tracing::info_span!(parent: None, "swarm.hop", otel.name="hop", stage="hop", sender_node, receiver_node, queue_wait_ms=tracing::field::Empty);
    record_hop_queue_wait(&span, queue_wait_ms);
    let _ = attach_parent(&span, parent);
    span
}
pub fn record_hop_queue_wait(span: &tracing::Span, queue_wait_ms: u64) {
    if let Ok(value) = i64::try_from(queue_wait_ms) {
        span.record("queue_wait_ms", value);
    }
}
fn correlated_span(c: &Correlation, stage: Stage, attempt: u64) -> tracing::Span {
    let span = tracing::info_span!(parent:None,"swarm.operation",otel.name=stage.as_str(),deployment_id=%c.deployment_id,run_id=c.run_id.as_deref().unwrap_or(""),goal_id=c.goal_id.as_deref().unwrap_or(""),task_id=c.task_id.as_deref().unwrap_or(""),intent_id=c.intent_id.as_deref().unwrap_or(""),agent_id=c.agent_id.as_deref().unwrap_or(""),node_id=c.node_id.as_deref().unwrap_or(""),incarnation=tracing::field::Empty,execution_attempt=tracing::field::Empty,stage=stage.as_str(),gen_ai.system=tracing::field::Empty,gen_ai.provider.name=tracing::field::Empty,gen_ai.request.model=tracing::field::Empty,gen_ai.usage.input_tokens=tracing::field::Empty,gen_ai.usage.output_tokens=tracing::field::Empty,gen_ai.cost.microusd=tracing::field::Empty);
    if let Some(value) = c.incarnation.and_then(|value| i64::try_from(value).ok()) {
        span.record("incarnation", value);
    }
    if (attempt != 0 || c.execution_attempt.is_some())
        && let Ok(value) = i64::try_from(attempt)
    {
        span.record("execution_attempt", value);
    }
    span
}
pub fn provider_attributes(provider: &str, model: &str, input: u64, output: u64, cost: u64) {
    let span = tracing::Span::current();
    if provider.len() <= 128 {
        span.record("gen_ai.system", provider);
        span.record("gen_ai.provider.name", provider);
    }
    if model.len() <= 128 {
        span.record("gen_ai.request.model", model);
    }
    for (name, value) in [
        ("gen_ai.usage.input_tokens", input),
        ("gen_ai.usage.output_tokens", output),
        ("gen_ai.cost.microusd", cost),
    ] {
        if let Ok(value) = i64::try_from(value) {
            span.record(name, value);
        }
    }
}
pub fn record(stage: Stage, outcome: Outcome, elapsed_ms: u64) {
    if let Some(m) = METRICS.get() {
        let labels = [
            KeyValue::new("stage", stage.as_str()),
            KeyValue::new("outcome", outcome.as_str()),
        ];
        m.operations.add(1, &labels);
        m.latency.record(elapsed_ms.min(3600000) as f64, &labels);
    }
}
pub fn set_gauge(metric: Gauge, value: u64) {
    if let Some(m) = METRICS.get() {
        m.gauge
            .record(value, &[KeyValue::new("metric", metric.as_str())]);
    }
}
pub fn record_counter(metric: Counter, delta: u64) {
    if let Some(m) = METRICS.get() {
        m.counters
            .add(delta, &[KeyValue::new("metric", metric.as_str())]);
    }
}
pub fn set_active_workers(value: u64) {
    set_gauge(Gauge::ActiveWorkers, value);
}
pub fn record_tokens(input: u64, output: u64) {
    if let Some(m) = METRICS.get() {
        m.tokens.add(input, &[KeyValue::new("direction", "input")]);
        m.tokens
            .add(output, &[KeyValue::new("direction", "output")]);
    }
}
pub fn record_money(spent: u64, held: u64) {
    set_gauge(Gauge::MoneySpentMicrousd, spent);
    set_gauge(Gauge::MoneyHeldMicrousd, held);
}

#[derive(Debug)]
struct SafeClient {
    client: Mutex<Option<reqwest::blocking::Client>>,
    base: reqwest::Url,
}
impl SafeClient {
    fn new(config: &DiagnosticsConfig) -> Result<Self> {
        let base = approved_endpoint(
            config
                .otlp_http_endpoint
                .as_deref()
                .context("diagnostic collector absent")?,
        )?;
        let timeout = Duration::from_millis(config.export_timeout_ms);
        let client = std::thread::spawn(move || {
            reqwest::blocking::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .timeout(timeout)
                .connect_timeout(timeout)
                .build()
        })
        .join()
        .map_err(|_| anyhow::anyhow!("diagnostic client initialization failed"))??;
        Ok(Self {
            client: Mutex::new(Some(client)),
            base,
        })
    }
    fn post(&self, path: &str, body: Vec<u8>) -> OTelSdkResult {
        if body.len() > 1024 * 1024 {
            return Err(OTelSdkError::InternalFailure(
                "diagnostic batch exceeds bound".into(),
            ));
        }
        let guard = self.client.lock().map_err(|_| {
            OTelSdkError::InternalFailure("diagnostic client lock unavailable".into())
        })?;
        let client = guard.as_ref().ok_or(OTelSdkError::AlreadyShutdown)?;
        let operation = || -> Result<()> {
            let url = self.base.join(path)?;
            let response = client
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()?;
            ensure!(
                response.status().is_success(),
                "collector rejected diagnostic export"
            );
            let mut bytes = Vec::new();
            response.take(65537).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 65536, "collector response exceeds bound");
            if !bytes.is_empty() {
                let reply: serde_json::Value = serde_json::from_slice(&bytes)?;
                if let Some(partial) = reply.get("partialSuccess") {
                    for key in ["rejectedSpans", "rejectedDataPoints"] {
                        let rejected = partial
                            .get(key)
                            .and_then(|v| {
                                v.as_u64()
                                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                            })
                            .unwrap_or(0);
                        ensure!(rejected == 0, "collector rejected diagnostic data");
                    }
                }
            }
            Ok(())
        };
        operation()
            .map_err(|_| OTelSdkError::InternalFailure("bounded diagnostic export failed".into()))
    }
}
impl Drop for SafeClient {
    fn drop(&mut self) {
        if let Ok(slot) = self.client.get_mut()
            && let Some(client) = slot.take()
        {
            let _ = std::thread::Builder::new()
                .name("swarm-otel-drop".into())
                .spawn(move || drop(client));
        }
    }
}
#[derive(Debug)]
struct JsonExporter {
    client: Arc<SafeClient>,
    resource: Resource,
}
impl SpanExporter for JsonExporter {
    async fn export(&self, mut batch: Vec<SpanData>) -> OTelSdkResult {
        for span in &mut batch {
            sanitize_span(span);
        }
        let request = opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest {
            resource_spans:
                opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope(
                    batch,
                    &(&self.resource).into(),
                ),
        };
        let body = serde_json::to_vec(&request)
            .map_err(|_| OTelSdkError::InternalFailure("diagnostic encoding failed".into()))?;
        self.client.post("/v1/traces", body)
    }
    fn set_resource(&mut self, r: &Resource) {
        self.resource = r.clone();
    }
}
impl PushMetricExporter for JsonExporter {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let request:opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest=metrics.into();
        let body = serde_json::to_vec(&request)
            .map_err(|_| OTelSdkError::InternalFailure("diagnostic encoding failed".into()))?;
        self.client.post("/v1/metrics", body)
    }
    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }
    fn shutdown_with_timeout(&self, _: Duration) -> OTelSdkResult {
        Ok(())
    }
    fn temporality(&self) -> Temporality {
        Temporality::Cumulative
    }
}
fn sanitize_span(span: &mut SpanData) {
    let old = span.span_context.clone();
    span.span_context = SpanContext::new(
        old.trace_id(),
        old.span_id(),
        old.trace_flags(),
        old.is_remote(),
        TraceState::default(),
    );
    const KEYS: &[&str] = &[
        "deployment_id",
        "run_id",
        "goal_id",
        "task_id",
        "intent_id",
        "agent_id",
        "node_id",
        "incarnation",
        "execution_attempt",
        "stage",
        "sender_node",
        "receiver_node",
        "queue_wait_ms",
        "gen_ai.system",
        "gen_ai.provider.name",
        "gen_ai.request.model",
        "gen_ai.usage.input_tokens",
        "gen_ai.usage.output_tokens",
        "gen_ai.cost.microusd",
    ];
    // The SDK retains repeated assignments; export the latest value once per attribute.
    let mut seen = std::collections::HashSet::new();
    span.attributes.reverse();
    span.attributes.retain(|v| {
        KEYS.contains(&v.key.as_str())
            && v.value.to_string().len() <= 256
            && seen.insert(v.key.clone())
    });
    span.attributes.reverse();
    span.events.events.clear();
    span.links.links.clear();
    if matches!(span.status, opentelemetry::trace::Status::Error { .. }) {
        span.status = opentelemetry::trace::Status::error("operation failed");
    }
    if ![
        "task.root",
        "task",
        "message",
        "provider",
        "tool",
        "hop",
        "scheduling",
        "state",
        "provider_queue",
        "lease",
        "cleanup",
        "reconcile",
        "raft",
    ]
    .contains(&span.name.as_ref())
    {
        span.name = "operation".into();
    }
}

pub struct Diagnostics {
    trace: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    shutdown_timeout: Duration,
}
impl Diagnostics {
    pub fn shutdown(mut self) -> Result<()> {
        let timeout = self.shutdown_timeout;
        let trace = self.trace.take();
        let meter = self.meter.take();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let first = trace.map(|t| t.shutdown_with_timeout(timeout)).transpose();
            let second = meter.map(|m| m.shutdown_with_timeout(timeout)).transpose();
            let result = first.and(second).map(|_| ()).map_err(anyhow::Error::from);
            let _ = send.send(result);
        });
        receive
            .recv_timeout(timeout)
            .map_err(|_| anyhow::anyhow!("diagnostic shutdown deadline reached"))?
    }
}
impl Drop for Diagnostics {
    fn drop(&mut self) {
        let trace = self.trace.take();
        let meter = self.meter.take();
        let timeout = self.shutdown_timeout;
        if trace.is_some() || meter.is_some() {
            let _ = std::thread::Builder::new()
                .name("swarm-otel-shutdown".into())
                .spawn(move || {
                    if let Some(t) = trace {
                        let _ = t.shutdown_with_timeout(timeout);
                    }
                    if let Some(m) = meter {
                        let _ = m.shutdown_with_timeout(timeout);
                    }
                });
        }
    }
}
pub fn initialize(config: &DiagnosticsConfig, node_id: &str) -> Result<Diagnostics> {
    config.validate()?;
    ensure!(
        node_id.len() <= 128 && !node_id.is_empty(),
        "diagnostic node resource invalid"
    );
    let timeout = Duration::from_millis(config.shutdown_timeout_ms);
    if config.otlp_http_endpoint.is_none() {
        return Ok(Diagnostics {
            trace: None,
            meter: None,
            shutdown_timeout: timeout,
        });
    }
    ensure!(
        TRACER.get().is_none() && METRICS.get().is_none(),
        "diagnostics already initialized"
    );
    let resource = Resource::builder_empty()
        .with_attributes([
            KeyValue::new("service.name", "solos-swarm"),
            KeyValue::new("service.instance.id", node_id.to_owned()),
        ])
        .build();
    let client = Arc::new(SafeClient::new(config)?);
    let exporter = || JsonExporter {
        client: client.clone(),
        resource: resource.clone(),
    };
    let batch = BatchConfigBuilder::default()
        .with_max_queue_size(config.queue_size)
        .with_max_export_batch_size(config.batch_size)
        .with_scheduled_delay(Duration::from_millis(config.export_interval_ms))
        .build();
    let trace = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_sampler(Sampler::AlwaysOn)
        .with_id_generator(CanonicalIds::default())
        .with_span_limits(SpanLimits {
            max_attributes_per_span: 24,
            max_events_per_span: 0,
            max_links_per_span: 0,
            max_attributes_per_event: 0,
            max_attributes_per_link: 0,
        })
        .with_span_processor(
            BatchSpanProcessor::builder(exporter())
                .with_batch_config(batch)
                .build(),
        )
        .build();
    let tracer = trace.tracer("solos-swarm");
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer.clone())
                .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                    matches!(
                        metadata.target(),
                        "solos_swarm::diagnostics" | "solos_swarm::cluster"
                    )
                })),
        ),
    )?;
    let meter = SdkMeterProvider::builder()
        .with_view(|_| {
            Some(
                opentelemetry_sdk::metrics::Stream::builder()
                    .with_cardinality_limit(128)
                    .build()
                    .expect("fixed nonzero metric cardinality"),
            )
        })
        .with_resource(resource.clone())
        .with_reader(
            PeriodicReader::builder(exporter())
                .with_interval(Duration::from_millis(config.export_interval_ms))
                .build(),
        )
        .build();
    let m = meter.meter("solos-swarm");
    let instruments = Instruments {
        operations: m.u64_counter("swarm.operations").build(),
        latency: m
            .f64_histogram("swarm.operation.duration")
            .with_unit("ms")
            .build(),
        gauge: m.u64_gauge("swarm.snapshot").build(),
        counters: m.u64_counter("swarm.control.events").build(),
        tokens: m.u64_counter("swarm.model.tokens").build(),
    };
    TRACER
        .set(tracer)
        .map_err(|_| anyhow::anyhow!("diagnostic tracer registration raced"))?;
    METRICS
        .set(instruments)
        .map_err(|_| anyhow::anyhow!("diagnostic metric registration raced"))?;
    Ok(Diagnostics {
        trace: Some(trace),
        meter: Some(meter),
        shutdown_timeout: timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_collector_rejects_ambient_or_external_egress() {
        for s in [
            "http://localhost:4318",
            "http://example.com:4318",
            "https://127.0.0.1:4318",
            "http://user:secret@127.0.0.1:4318",
            "http://127.0.0.1:4318/v1/traces",
            "http://127.0.0.1:4318?token=x",
        ] {
            assert!(approved_endpoint(s).is_err(), "{s}");
        }
        assert!(approved_endpoint("http://127.0.0.1:4318").is_ok());
        assert!(approved_endpoint("http://[::1]:4318").is_ok());
    }
    #[tokio::test]
    async fn canonical_context_propagates_without_export_or_ambient_context() {
        let t = TraceContext::for_task("d", "g", "t");
        let expected = t.traceparent();
        assert!(current_trace_headers().is_none());
        with_trace_context(Some(t), async {
            assert_eq!(current_trace_headers().unwrap().0, expected);
        })
        .await;
        assert!(current_trace_headers().is_none());
    }
    fn collector(
        count: usize,
    ) -> (
        String,
        std::thread::JoinHandle<Vec<(String, serde_json::Value)>>,
    ) {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut frames = vec![];
            for _ in 0..count {
                let (mut socket, _) = server.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = vec![];
                let mut one = [0u8; 1];
                while !bytes.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut one).unwrap();
                    bytes.push(one[0]);
                    assert!(bytes.len() < 8192);
                }
                let headers = String::from_utf8(bytes).unwrap();
                assert!(!headers.to_ascii_lowercase().contains("authorization:"));
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap();
                assert!(length <= 1024 * 1024);
                let mut data = vec![0; length];
                socket.read_exact(&mut data).unwrap();
                frames.push((
                    headers.lines().next().unwrap().into(),
                    serde_json::from_slice(&data).unwrap(),
                ));
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
            }
            frames
        });
        (format!("http://{address}"), handle)
    }
    #[test]
    fn actual_otlp_json_uses_canonical_root_sanitizes_secrets_and_has_bounded_metric_labels() {
        use opentelemetry::trace::Span;
        let (endpoint, receiver) = collector(3);
        let config = DiagnosticsConfig {
            otlp_http_endpoint: Some(endpoint),
            ..Default::default()
        };
        let client = Arc::new(SafeClient::new(&config).unwrap());
        let resource = Resource::builder_empty()
            .with_attributes([KeyValue::new("service.name", "solos-swarm")])
            .build();
        let exporter = || JsonExporter {
            client: client.clone(),
            resource: resource.clone(),
        };
        let provider = SdkTracerProvider::builder()
            .with_resource(resource.clone())
            .with_id_generator(CanonicalIds::default())
            .with_sampler(Sampler::AlwaysOn)
            .with_simple_exporter(exporter())
            .build();
        let tracer = provider.tracer("test");
        let context = TraceContext::for_task("deployment", "goal", "task");
        let mut root = ROOT_IDS.with(|ids| {
            ids.set(Some((
                TraceId::from_hex(&context.trace_id).unwrap(),
                SpanId::from_hex(&context.span_id).unwrap(),
            )));
            let root = tracer.start_with_context("task.root", &Context::new());
            ids.set(None);
            root
        });
        root.set_attribute(KeyValue::new("task_id", "task"));
        root.set_attribute(KeyValue::new("secret", "PRIVATE_EXPORT_CANARY"));
        root.add_event("PRIVATE_EXPORT_CANARY", vec![]);
        root.set_status(opentelemetry::trace::Status::error("PRIVATE_EXPORT_CANARY"));
        root.end();
        provider.shutdown().unwrap();
        let meter = SdkMeterProvider::builder()
            .with_resource(resource.clone())
            .with_reader(
                PeriodicReader::builder(exporter())
                    .with_interval(Duration::from_secs(60))
                    .build(),
            )
            .build();
        let m = meter.meter("test");
        let counter = m.u64_counter("swarm.operations").build();
        counter.add(
            1,
            &[
                KeyValue::new("stage", Stage::Provider.as_str()),
                KeyValue::new("outcome", Outcome::Success.as_str()),
            ],
        );
        meter.force_flush().unwrap();
        meter.shutdown().unwrap();
        let frames = receiver.join().unwrap();
        assert_eq!(frames.len(), 3);
        assert!(frames[0].0.starts_with("POST /v1/traces "));
        assert!(frames[1].0.starts_with("POST /v1/metrics "));
        let span = &frames[0].1["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["traceId"], context.trace_id);
        assert_eq!(span["spanId"], context.span_id);
        assert_eq!(span["name"], "task.root");
        let bytes = serde_json::to_string(&frames).unwrap();
        assert!(!bytes.contains("PRIVATE_EXPORT_CANARY"));
        assert!(bytes.contains("task_id"));
        assert!(bytes.contains("provider"));
    }
    #[test]
    fn collector_redirect_is_not_followed_and_is_not_retried() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        other.set_nonblocking(true).unwrap();
        let other_address = other.local_addr().unwrap();
        let receiver = std::thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut one = [0; 1];
            let mut bytes = vec![];
            while !bytes.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut one).unwrap();
                bytes.push(one[0]);
            }
            socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://{other_address}/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
            server.set_nonblocking(true).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            assert!(server.accept().is_err());
        });
        let client = SafeClient::new(&DiagnosticsConfig {
            otlp_http_endpoint: Some(format!("http://{address}")),
            ..Default::default()
        })
        .unwrap();
        assert!(client.post("/v1/traces", b"{}".to_vec()).is_err());
        receiver.join().unwrap();
        assert!(other.accept().is_err());
    }
    #[test]
    fn ambient_env_probe_child() {
        let Ok(endpoint) = std::env::var("SOLOS_OTLP_CHILD_ENDPOINT") else {
            return;
        };
        let config = DiagnosticsConfig {
            otlp_http_endpoint: Some(endpoint),
            export_interval_ms: 60000,
            ..Default::default()
        };
        let diagnostics = initialize(&config, "test-node").unwrap();
        let mut correlation = Correlation::for_actor("deployment", "operator");
        correlation.goal_id = Some("goal".into());
        correlation.task_id = Some("task".into());
        correlation.incarnation = Some(2);
        correlation.execution_attempt = Some(3);
        correlation.trace_context = Some(TraceContext::for_task("deployment", "goal", "task"));
        let root = task_span(&correlation);
        let provider = operation_span(&correlation, Stage::Provider, 3);
        {
            let _entered = provider.enter();
            provider_attributes("fixture", "pinned-model", 4, 3, 7);
        }
        drop(provider);
        let mut overflow = correlation.clone();
        overflow.incarnation = Some(u64::MAX);
        overflow.execution_attempt = Some(u64::MAX);
        let overflow = operation_span(&overflow, Stage::Tool, u64::MAX);
        {
            let _entered = overflow.enter();
            provider_attributes("fixture", "pinned-model", u64::MAX, u64::MAX, u64::MAX);
        }
        drop(overflow);
        let hop = hop_span(
            correlation.trace_context.as_ref().unwrap(),
            "sender-n1",
            "receiver-n2",
            0,
        );
        record_hop_queue_wait(&hop, 37);
        let denied = hop_span(
            correlation.trace_context.as_ref().unwrap(),
            &"PRIVATE_AMBIENT_CANARY".repeat(8),
            "receiver-n2",
            0,
        );
        assert!(denied.is_disabled());
        {
            let _untrusted = tracing::info_span!(target:"thirdparty::unknown","PRIVATE_AMBIENT_CANARY",payload="PRIVATE_AMBIENT_CANARY");
        }
        drop(hop);
        drop(root);
        record(Stage::Provider, Outcome::Success, 1);
        diagnostics.shutdown().unwrap();
    }
    #[test]
    fn initialization_does_not_export_ambient_headers_resources_or_endpoints() {
        let (endpoint, receiver) = collector(2);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::tests::ambient_env_probe_child",
                "--test-threads=1",
            ])
            .env("SOLOS_OTLP_CHILD_ENDPOINT", endpoint)
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://example.invalid:1")
            .env(
                "OTEL_EXPORTER_OTLP_HEADERS",
                "authorization=PRIVATE_AMBIENT_CANARY",
            )
            .env(
                "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
                "secret=PRIVATE_AMBIENT_CANARY",
            )
            .env("OTEL_RESOURCE_ATTRIBUTES", "private=PRIVATE_AMBIENT_CANARY")
            .env("HTTP_PROXY", "http://example.invalid:1")
            .status()
            .unwrap();
        assert!(status.success());
        let frames = receiver.join().unwrap();
        let bytes = serde_json::to_string(&frames).unwrap();
        assert!(!bytes.contains("PRIVATE_AMBIENT_CANARY"));
        assert!(frames.iter().any(|f| f.0.starts_with("POST /v1/traces ")));
        assert!(frames.iter().any(|f| f.0.starts_with("POST /v1/metrics ")));
        let roots = frames
            .iter()
            .flat_map(|f| f.1["resourceSpans"].as_array().into_iter().flatten())
            .flat_map(|r| r["scopeSpans"].as_array().into_iter().flatten())
            .flat_map(|s| s["spans"].as_array().into_iter().flatten())
            .filter(|s| s["name"] == "task.root")
            .collect::<Vec<_>>();
        let names = frames
            .iter()
            .flat_map(|f| f.1["resourceSpans"].as_array().into_iter().flatten())
            .flat_map(|r| r["scopeSpans"].as_array().into_iter().flatten())
            .flat_map(|s| s["spans"].as_array().into_iter().flatten())
            .map(|span| span["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 5);
        assert!(
            names.iter().all(|name| matches!(
                name.as_str(),
                "task" | "task.root" | "hop" | "provider" | "tool"
            )),
            "foreign/freeform span exported"
        );
        assert_eq!(roots.len(), 1);
        assert_eq!(
            roots[0]["traceId"],
            TraceContext::for_task("deployment", "goal", "task").trace_id
        );
        let hop = frames
            .iter()
            .flat_map(|f| f.1["resourceSpans"].as_array().into_iter().flatten())
            .flat_map(|r| r["scopeSpans"].as_array().into_iter().flatten())
            .flat_map(|s| s["spans"].as_array().into_iter().flatten())
            .find(|span| span["name"] == "hop")
            .unwrap();
        assert_eq!(hop["traceId"], roots[0]["traceId"]);
        let attribute = |key: &str| {
            &hop["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|attribute| attribute["key"] == key)
                .unwrap()["value"]
        };
        assert_eq!(attribute("sender_node")["stringValue"], "sender-n1");
        assert_eq!(attribute("receiver_node")["stringValue"], "receiver-n2");
        assert_eq!(attribute("queue_wait_ms")["intValue"], "37");
        assert_eq!(
            hop["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|attribute| attribute["key"] == "queue_wait_ms")
                .count(),
            1,
            "queue wait update must replace its initial value"
        );
        let spans = frames
            .iter()
            .flat_map(|f| f.1["resourceSpans"].as_array().into_iter().flatten())
            .flat_map(|r| r["scopeSpans"].as_array().into_iter().flatten())
            .flat_map(|s| s["spans"].as_array().into_iter().flatten())
            .collect::<Vec<_>>();
        let provider = spans
            .iter()
            .find(|span| span["name"] == "provider")
            .unwrap();
        for (key, value) in [
            ("incarnation", "2"),
            ("execution_attempt", "3"),
            ("gen_ai.usage.input_tokens", "4"),
            ("gen_ai.usage.output_tokens", "3"),
            ("gen_ai.cost.microusd", "7"),
        ] {
            let attribute = provider["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|attribute| attribute["key"] == key)
                .unwrap();
            assert_eq!(attribute["value"]["intValue"], value);
            assert!(attribute["value"].get("stringValue").is_none());
        }
        let overflow = spans.iter().find(|span| span["name"] == "tool").unwrap();
        assert!(
            overflow["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|attribute| {
                    !matches!(
                        attribute["key"].as_str().unwrap(),
                        "incarnation"
                            | "execution_attempt"
                            | "gen_ai.usage.input_tokens"
                            | "gen_ai.usage.output_tokens"
                            | "gen_ai.cost.microusd"
                    )
                })
        );
        let metric = frames
            .iter()
            .find(|f| f.0.starts_with("POST /v1/metrics "))
            .unwrap();
        assert!(
            !serde_json::to_string(&metric.1)
                .unwrap()
                .contains("task_id")
        );
        assert!(
            !serde_json::to_string(&metric.1)
                .unwrap()
                .contains("sender_node")
        );
        assert!(
            !serde_json::to_string(&metric.1)
                .unwrap()
                .contains("receiver_node")
        );
    }
}
