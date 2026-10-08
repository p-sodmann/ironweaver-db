//! OpenTelemetry traces over OTLP (step 16g, [ADR 0057](../../../documentation/adr/0057-traces.md);
//! feature `otel`).
//!
//! The spans are opened in the library crates with `tracing` (target
//! `iwdb::trace`). This module turns them into OpenTelemetry spans
//! (`tracing-opentelemetry`, [`Tracing::layer`]), samples them (parent-based,
//! with `[tracing] sample_ratio` for new roots), and exports them:
//!
//! - **Batching** ([`Batch`]): ended spans go to a bounded queue
//!   ([`QUEUE`]); a thread of its own exports batches of up to [`BATCH`]
//!   every [`DELAY`], or sooner when a batch is full, each within
//!   [`EXPORT_TIMEOUT`]. A full queue drops the new span, a failed export
//!   its batch: a request never waits for the collector. The counts go to
//!   [`SpanCounters`] (the metrics).
//! - **Export** ([`Otlp`]): OTLP/gRPC (`TraceService/Export`) or protobuf
//!   over HTTP (`POST .../v1/traces`), plain or TLS (`https://`, the
//!   system's roots), on a small tokio runtime owned by the export thread.
//!   No `OTEL_*` variable is read.
//! - **Context** ([`remote_context`]): the W3C `traceparent` and
//!   `tracestate` of a request; a malformed one is ignored.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use iwdb_query::trace::SpanCounters;
use opentelemetry::propagation::{Extractor, TextMapPropagator};
use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
use opentelemetry::{Context, KeyValue};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_client::TraceServiceClient;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider, Span, SpanData, SpanExporter, SpanLimits, SpanProcessor};
use prost::Message;
use tonic::transport::{Channel, Endpoint};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::registry::LookupSpan;

use crate::client::ClientTls;
use crate::client::https::Connector;
use crate::config::{Config, OtlpProtocol};

/// Spans waiting for export, at most; more are dropped (`queue_full`).
pub const QUEUE: usize = 2048;
/// Spans per export, at most.
pub const BATCH: usize = 512;
/// How long a span waits for a batch to fill before it is exported anyway.
pub const DELAY: Duration = Duration::from_secs(5);
/// How long one export may take; then its batch is dropped
/// (`export_failed`).
pub const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What `[tracing]` says, checked ([`Config::check`] did).
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub endpoint: String,
    pub protocol: OtlpProtocol,
    pub sample_ratio: f64,
    pub service_name: String,
    pub headers: Vec<(String, String)>,
}

impl Settings {
    pub fn of(config: &Config) -> Result<Settings, String> {
        Ok(Settings {
            endpoint: config.tracing_endpoint(),
            protocol: config.tracing.protocol,
            sample_ratio: config.tracing.sample_ratio,
            service_name: config.tracing.service_name.clone(),
            headers: config.tracing_headers()?,
        })
    }

    /// `service.name` and `service.version`; nothing else about the host,
    /// and nothing from the environment.
    pub fn resource(&self) -> Resource {
        Resource::builder_empty()
            .with_service_name(self.service_name.clone())
            .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
            .build()
    }
}

/// The tracer provider and its batching: [`layer`](Self::layer) for the
/// subscriber, [`shutdown`](Self::shutdown) at the end.
pub struct Tracing {
    provider: SdkTracerProvider,
}

impl std::fmt::Debug for Tracing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracing").finish_non_exhaustive()
    }
}

impl Tracing {
    /// Export to the collector `settings` names, counting into `counters`.
    /// Nothing connects until the first export. Errors: an endpoint or TLS
    /// setup that can't be used, a thread that can't start.
    pub fn start(settings: &Settings, counters: Arc<SpanCounters>) -> Result<Tracing, String> {
        let (endpoint, protocol, headers) = (settings.endpoint.clone(), settings.protocol, settings.headers.clone());
        let resource = ResourceAttributesWithSchema::from(&settings.resource());
        // Checked here, and built again on the export thread's runtime
        // (tonic's channel and hyper's client spawn their tasks there)
        Otlp::check(&endpoint, protocol)?;
        let make = move || Otlp::new(&endpoint, protocol, &headers, resource);
        Tracing::with_exporter(settings, counters, make)
    }

    /// [`start`](Self::start) with another exporter (tests: the SDK's
    /// in-memory one). `make` runs on the export thread, in its runtime.
    pub fn with_exporter<E, M>(settings: &Settings, counters: Arc<SpanCounters>, make: M) -> Result<Tracing, String>
    where
        E: SpanExporter + 'static,
        M: FnOnce() -> Result<E, String> + Send + 'static,
    {
        let batch = Batch::start(counters, make)?;
        let sampler = Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(settings.sample_ratio)));
        let provider = SdkTracerProvider::builder()
            .with_sampler(sampler)
            // Explicit, so that no OTEL_SPAN_* variable changes them
            .with_span_limits(SpanLimits::default())
            .with_resource(settings.resource())
            .with_span_processor(batch)
            .build();
        Ok(Tracing { provider })
    }

    /// The layer that turns the trace spans (target `iwdb::trace`, and only
    /// those) into OpenTelemetry spans.
    pub fn layer<S>(&self) -> impl Layer<S> + Send + Sync + 'static
    where
        S: tracing::Subscriber + Send + Sync + for<'a> LookupSpan<'a>,
    {
        ACTIVE.store(true, Ordering::Release);
        let tracer = self.provider.tracer("iwdb");
        tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_threads(false)
            .with_location(false)
            .with_target(false)
            .with_filter(Targets::new().with_target(iwdb_storage::trace::TARGET, LevelFilter::INFO))
    }

    /// Export what is queued, waiting at most `timeout`, and stop. What
    /// doesn't make it is dropped (and counted). Idempotent.
    pub fn shutdown(&self, timeout: Duration) {
        if let Err(e) = self.provider.shutdown_with_timeout(timeout) {
            match e {
                OTelSdkError::AlreadyShutdown => {}
                e => tracing::warn!(error = %e, "flushing the traces on shutdown"),
            }
        }
    }

    /// Export what is queued now, waiting at most [`EXPORT_TIMEOUT`].
    pub fn flush(&self) -> Result<(), String> {
        self.provider.force_flush().map_err(|e| e.to_string())
    }
}

/// Whether a trace layer was made ([`Tracing::layer`]): the gate reads
/// trace context only then.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether this process exports traces.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// The W3C trace context of a request's headers (gRPC metadata are HTTP/2
/// headers): its `traceparent` and `tracestate`. `None` without a valid
/// `traceparent` (missing or malformed): the request starts a new trace.
pub fn remote_context(headers: &http::HeaderMap) -> Option<Context> {
    struct Headers<'a>(&'a http::HeaderMap);
    impl Extractor for Headers<'_> {
        fn get(&self, key: &str) -> Option<&str> {
            self.0.get(key).and_then(|v| v.to_str().ok())
        }
        fn keys(&self) -> Vec<&str> {
            self.0.keys().map(http::HeaderName::as_str).collect()
        }
    }
    headers.get("traceparent")?;
    let cx = TraceContextPropagator::new().extract(&Headers(headers));
    cx.span().span_context().is_valid().then_some(cx)
}

/// The queue between ending spans and the export thread.
struct Queue {
    spans: VecDeque<SpanData>,
    /// Flushes asked for, and done (the thread exported what was queued
    /// when one was asked).
    asked: u64,
    done: u64,
    stop: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    /// The thread waits on it for spans, a flush or the stop; flushes wait
    /// on it for the thread.
    changed: Condvar,
    counters: Arc<SpanCounters>,
}

/// The span processor: a bounded queue and an export thread (see the module
/// docs). The SDK's `BatchSpanProcessor` keeps its drop count to itself and
/// exports without a tokio runtime, so this one is ours.
#[derive(Debug)]
struct Batch {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl Batch {
    fn start<E, M>(counters: Arc<SpanCounters>, make: M) -> Result<Batch, String>
    where
        E: SpanExporter + 'static,
        M: FnOnce() -> Result<E, String> + Send + 'static,
    {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue { spans: VecDeque::new(), asked: 0, done: 0, stop: false }),
            changed: Condvar::new(),
            counters,
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("starting the trace exporter's runtime: {}", e))?;
        let exporter = {
            let _entered = runtime.enter();
            make()?
        };
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("iwdb-trace-export".into())
            .spawn(move || export_loop(&thread_shared, &runtime, &exporter))
            .map_err(|e| format!("starting the trace exporter's thread: {}", e))?;
        Ok(Batch { shared, thread: Mutex::new(Some(thread)) })
    }

    /// Ask the thread to export what is queued and wait until it has, at
    /// most `timeout`.
    fn flush(&self, timeout: Duration) -> OTelSdkResult {
        let deadline = Instant::now() + timeout;
        let mut queue = lock(&self.shared.queue);
        if queue.stop {
            return Err(OTelSdkError::AlreadyShutdown);
        }
        queue.asked += 1;
        let asked = queue.asked;
        self.shared.changed.notify_all();
        while queue.done < asked {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(OTelSdkError::Timeout(timeout));
            }
            queue = self.shared.changed.wait_timeout(queue, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        Ok(())
    }
}

impl SpanProcessor for Batch {
    fn on_start(&self, _span: &mut Span, _cx: &Context) {}

    fn on_end(&self, span: SpanData) {
        if !span.span_context.is_sampled() {
            return;
        }
        let mut queue = lock(&self.shared.queue);
        if queue.stop || queue.spans.len() >= QUEUE {
            drop(queue);
            self.shared.counters.queue_full(1);
            return;
        }
        queue.spans.push_back(span);
        let full = queue.spans.len() >= BATCH;
        drop(queue);
        if full {
            self.shared.changed.notify_all();
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.flush(EXPORT_TIMEOUT)
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        let Some(thread) = lock(&self.thread).take() else { return Err(OTelSdkError::AlreadyShutdown) };
        let flushed = self.flush(timeout);
        lock(&self.shared.queue).stop = true;
        self.shared.changed.notify_all();
        // After a timed-out flush the thread may still be in an export; it
        // stops after it (at most EXPORT_TIMEOUT), detached
        if flushed.is_ok() {
            let _ = thread.join();
        }
        // What is still queued is dropped
        let left = std::mem::take(&mut lock(&self.shared.queue).spans);
        self.shared.counters.queue_full(left.len() as u64);
        flushed
    }
}

/// The export thread: wait for a full batch, a flush, the delay or the
/// stop; export in batches; count; say when exports start failing and when
/// they work again.
fn export_loop<E: SpanExporter>(shared: &Shared, runtime: &tokio::runtime::Runtime, exporter: &E) {
    let mut failing = false;
    let mut last = Instant::now();
    loop {
        let (batches, asked, stop) = {
            let mut queue = lock(&shared.queue);
            loop {
                let due = !queue.spans.is_empty() && last.elapsed() >= DELAY;
                if queue.stop || queue.asked > queue.done || queue.spans.len() >= BATCH || due {
                    break;
                }
                let wait = if queue.spans.is_empty() { DELAY } else { DELAY.saturating_sub(last.elapsed()) };
                queue = shared.changed.wait_timeout(queue, wait).unwrap_or_else(PoisonError::into_inner).0;
            }
            // A flush or the stop takes everything; otherwise full batches,
            // or what is there once the delay has passed
            let all = queue.stop || queue.asked > queue.done || last.elapsed() >= DELAY;
            let mut batches = Vec::new();
            while queue.spans.len() >= BATCH || (all && !queue.spans.is_empty()) {
                let n = queue.spans.len().min(BATCH);
                batches.push(queue.spans.drain(..n).collect::<Vec<_>>());
            }
            (batches, queue.asked, queue.stop)
        };
        for batch in batches {
            let n = batch.len() as u64;
            let result = runtime.block_on(async { tokio::time::timeout(EXPORT_TIMEOUT, exporter.export(batch)).await });
            match result {
                Ok(Ok(())) => {
                    shared.counters.exported(n);
                    if failing {
                        failing = false;
                        tracing::info!("trace export works again");
                    }
                }
                Ok(Err(e)) => {
                    shared.counters.export_failed(n);
                    if !failing {
                        failing = true;
                        tracing::warn!(error = %e, "trace export failed; spans are dropped until the collector takes them");
                    }
                }
                Err(_) => {
                    shared.counters.export_failed(n);
                    if !failing {
                        failing = true;
                        tracing::warn!(
                            timeout_ms = EXPORT_TIMEOUT.as_millis() as u64,
                            "trace export timed out; spans are dropped until the collector takes them"
                        );
                    }
                }
            }
        }
        last = Instant::now();
        let mut queue = lock(&shared.queue);
        queue.done = queue.done.max(asked);
        shared.changed.notify_all();
        if stop && queue.spans.is_empty() {
            return;
        }
    }
}

/// The OTLP exporter: gRPC or protobuf over HTTP.
pub struct Otlp {
    transport: Transport,
    resource: ResourceAttributesWithSchema,
}

enum Transport {
    Grpc { client: TraceServiceClient<Channel>, headers: tonic::metadata::MetadataMap },
    // Boxed: the HTTP client is much larger than the gRPC one
    Http { client: Box<Client<Connector, Full<Bytes>>>, uri: http::Uri, headers: http::HeaderMap },
}

impl std::fmt::Debug for Otlp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.transport {
            Transport::Grpc { .. } => "grpc",
            Transport::Http { .. } => "http/protobuf",
        };
        f.debug_struct("Otlp").field("protocol", &kind).finish_non_exhaustive()
    }
}

impl Otlp {
    /// The URI spans are sent to: for HTTP, `/v1/traces` is added to an
    /// endpoint without a path.
    fn uri(endpoint: &str, protocol: OtlpProtocol) -> Result<http::Uri, String> {
        let uri: http::Uri = endpoint.parse().map_err(|_| format!("[tracing] endpoint {:?} isn't a URL", endpoint))?;
        if protocol == OtlpProtocol::HttpProtobuf && matches!(uri.path(), "" | "/") {
            let base = endpoint.trim_end_matches('/');
            return format!("{}/v1/traces", base).parse().map_err(|_| format!("[tracing] endpoint {:?}", endpoint));
        }
        Ok(uri)
    }

    /// The endpoint and the TLS setup can be used.
    fn check(endpoint: &str, protocol: OtlpProtocol) -> Result<(), String> {
        let uri = Otlp::uri(endpoint, protocol)?;
        if uri.scheme_str() == Some("https") {
            match protocol {
                OtlpProtocol::Grpc => {
                    crate::client::tonic_tls(&ClientTls::default()).map(|_| ()).map_err(|e| e.to_string())
                }
                OtlpProtocol::HttpProtobuf => {
                    ClientTls::default().rustls_config(&[b"http/1.1"]).map(|_| ()).map_err(|e| e.to_string())
                }
            }?;
        }
        Ok(())
    }

    /// An exporter to `endpoint`; call it inside a tokio runtime. Nothing
    /// connects before the first export.
    fn new(
        endpoint: &str,
        protocol: OtlpProtocol,
        headers: &[(String, String)],
        resource: ResourceAttributesWithSchema,
    ) -> Result<Otlp, String> {
        let uri = Otlp::uri(endpoint, protocol)?;
        let https = uri.scheme_str() == Some("https");
        let transport = match protocol {
            OtlpProtocol::Grpc => {
                let mut channel = Endpoint::from_shared(endpoint.to_owned())
                    .map_err(|e| format!("[tracing] endpoint: {}", e))?
                    .connect_timeout(EXPORT_TIMEOUT);
                if https {
                    let tls = crate::client::tonic_tls(&ClientTls::default()).map_err(|e| e.to_string())?;
                    channel = channel.tls_config(tls).map_err(|e| format!("[tracing] TLS: {}", e))?;
                }
                let mut map = tonic::metadata::MetadataMap::new();
                for (name, value) in headers {
                    let key = tonic::metadata::MetadataKey::from_bytes(name.as_bytes())
                        .map_err(|_| format!("[tracing] headers: {:?} isn't a valid header name", name))?;
                    let value = value.parse().map_err(|_| format!("[tracing] headers: {}'s value", name))?;
                    map.insert(key, value);
                }
                Transport::Grpc { client: TraceServiceClient::new(channel.connect_lazy()), headers: map }
            }
            OtlpProtocol::HttpProtobuf => {
                let tls = if https {
                    let config = ClientTls::default().rustls_config(&[b"http/1.1"]).map_err(|e| e.to_string())?;
                    Some(Arc::new(config))
                } else {
                    None
                };
                let client = Box::new(Client::builder(TokioExecutor::new()).build(Connector::new(tls)));
                let mut map = http::HeaderMap::new();
                for (name, value) in headers {
                    let name = http::HeaderName::from_bytes(name.as_bytes())
                        .map_err(|_| format!("[tracing] headers: {:?} isn't a valid header name", name))?;
                    let value = http::HeaderValue::from_str(value)
                        .map_err(|_| format!("[tracing] headers: {}'s value", name))?;
                    map.insert(name, value);
                }
                Transport::Http { client, uri, headers: map }
            }
        };
        Ok(Otlp { transport, resource })
    }
}

impl SpanExporter for Otlp {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let request =
            ExportTraceServiceRequest { resource_spans: group_spans_by_resource_and_scope(batch, &self.resource) };
        let failed = |e: String| OTelSdkError::InternalFailure(e);
        match &self.transport {
            Transport::Grpc { client, headers } => {
                let mut call = tonic::Request::new(request);
                *call.metadata_mut() = headers.clone();
                client.clone().export(call).await.map_err(|s| failed(format!("{}: {}", s.code(), s.message())))?;
                Ok(())
            }
            Transport::Http { client, uri, headers } => {
                let mut call = http::Request::post(uri.clone())
                    .header(http::header::CONTENT_TYPE, "application/x-protobuf")
                    .body(Full::new(Bytes::from(request.encode_to_vec())))
                    .map_err(|e| failed(e.to_string()))?;
                call.headers_mut().extend(headers.clone());
                let response = client.request(call).await.map_err(|e| failed(e.to_string()))?;
                if !response.status().is_success() {
                    return Err(failed(format!("the collector answered {}", response.status())));
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use tracing_subscriber::prelude::*;

    use super::*;

    fn settings(sample_ratio: f64) -> Settings {
        Settings {
            endpoint: OtlpProtocol::Grpc.default_endpoint().into(),
            protocol: OtlpProtocol::Grpc,
            sample_ratio,
            service_name: "iwdb-test".into(),
            headers: Vec::new(),
        }
    }

    /// `n` request trees (a request with two children) on this thread,
    /// traced by `tracing`.
    fn spans(tracing: &Tracing, n: usize) {
        let subscriber = tracing_subscriber::registry().with(tracing.layer());
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..n {
                let request = iwdb_storage::trace_span!("request", otel.name = "Find");
                let _entered = request.enter();
                drop(iwdb_storage::trace_span!("iwdb.queue"));
                drop(iwdb_storage::trace_span!("iwdb.execute"));
            }
        });
    }

    fn in_memory(sample_ratio: f64) -> (Tracing, InMemorySpanExporter, Arc<SpanCounters>) {
        let exporter = InMemorySpanExporter::default();
        let counters = Arc::new(SpanCounters::default());
        let export = exporter.clone();
        let tracing = Tracing::with_exporter(&settings(sample_ratio), counters.clone(), move || Ok(export)).unwrap();
        (tracing, exporter, counters)
    }

    #[test]
    fn sampling_at_0_exports_nothing_and_at_1_everything() {
        for (ratio, expected) in [(0.0, 0), (1.0, 300)] {
            let (tracing, exporter, counters) = in_memory(ratio);
            spans(&tracing, 100);
            tracing.flush().unwrap();
            assert_eq!(exporter.get_finished_spans().unwrap().len(), expected, "ratio {}", ratio);
            assert_eq!(counters.read(), (expected as u64, 0, 0));
            tracing.shutdown(Duration::from_secs(5));
        }
    }

    #[test]
    fn shutdown_exports_what_is_queued() {
        let (tracing, exporter, counters) = in_memory(1.0);
        spans(&tracing, 10);
        // Well before the batch's delay: only the shutdown's flush sends them
        assert!(exporter.get_finished_spans().unwrap().is_empty());
        tracing.shutdown(Duration::from_secs(5));
        assert_eq!(exporter.get_finished_spans().unwrap().len(), 30);
        assert_eq!(counters.read(), (30, 0, 0));
        // Twice is fine; spans after it go nowhere (the provider has
        // stopped, so they don't reach the queue)
        tracing.shutdown(Duration::from_secs(1));
        spans(&tracing, 1);
        assert_eq!(exporter.get_finished_spans().unwrap().len(), 30);
    }

    #[test]
    fn a_full_queue_drops_new_spans_and_counts_them() {
        // An exporter that waits until released: the queue fills meanwhile
        #[derive(Debug)]
        struct Held(Arc<(Mutex<bool>, Condvar)>);
        impl SpanExporter for Held {
            async fn export(&self, _batch: Vec<SpanData>) -> OTelSdkResult {
                let (released, changed) = &*self.0;
                let mut released = lock(released);
                while !*released {
                    released = changed.wait(released).unwrap_or_else(PoisonError::into_inner);
                }
                Ok(())
            }
        }
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let counters = Arc::new(SpanCounters::default());
        let held = Held(gate.clone());
        let tracing = Tracing::with_exporter(&settings(1.0), counters.clone(), move || Ok(held)).unwrap();
        // The first full batch goes to the held exporter, then the queue fills
        let n = (BATCH + QUEUE) / 3 + 400;
        let start = Instant::now();
        spans(&tracing, n);
        assert!(start.elapsed() < Duration::from_secs(5), "ending spans waited: {:?}", start.elapsed());
        let (_, dropped, _) = counters.read();
        assert!(dropped > 0, "{:?}", counters.read());
        *lock(&gate.0) = true;
        gate.1.notify_all();
        tracing.shutdown(Duration::from_secs(10));
        let (exported, dropped, failed) = counters.read();
        assert_eq!((exported + dropped, failed), (3 * n as u64, 0));
    }

    /// A collector that isn't there: ending spans doesn't wait, exports
    /// fail and are counted, and the shutdown still ends in time.
    #[test]
    fn a_collector_that_is_down_costs_requests_nothing() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        for protocol in [OtlpProtocol::Grpc, OtlpProtocol::HttpProtobuf] {
            let settings = Settings { endpoint: format!("http://127.0.0.1:{}", port), protocol, ..settings(1.0) };
            let counters = Arc::new(SpanCounters::default());
            let tracing = Tracing::start(&settings, counters.clone()).unwrap();
            let start = Instant::now();
            spans(&tracing, 1000);
            assert!(start.elapsed() < Duration::from_secs(5), "{:?}: {:?}", protocol, start.elapsed());
            let start = Instant::now();
            tracing.shutdown(Duration::from_secs(10));
            assert!(start.elapsed() < Duration::from_secs(10), "{:?}", start.elapsed());
            // Every span is dropped and counted: by a failed export, or at
            // the queue while a slow failing export holds it up
            let (exported, queue_full, failed) = counters.read();
            assert_eq!((exported, queue_full + failed), (0, 3000), "{:?}", protocol);
            assert!(failed > 0, "{:?}: {:?}", protocol, counters.read());
        }
    }

    #[test]
    fn http_endpoints_get_the_traces_path() {
        let uri = |e| Otlp::uri(e, OtlpProtocol::HttpProtobuf).unwrap().to_string();
        assert_eq!(uri("http://c:4318"), "http://c:4318/v1/traces");
        assert_eq!(uri("http://c:4318/"), "http://c:4318/v1/traces");
        assert_eq!(uri("https://c/otlp/v1/traces"), "https://c/otlp/v1/traces");
        assert_eq!(Otlp::uri("http://c:4317", OtlpProtocol::Grpc).unwrap().to_string(), "http://c:4317/");
    }

    #[test]
    fn traceparent_is_read_and_a_malformed_one_ignored() {
        let headers = |value: &str| {
            let mut map = http::HeaderMap::new();
            map.insert("traceparent", value.parse().unwrap());
            map.insert("tracestate", "a=1".parse().unwrap());
            map
        };
        let cx = remote_context(&headers("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01")).unwrap();
        let span = cx.span().span_context().clone();
        assert_eq!(span.trace_id().to_string(), "0af7651916cd43dd8448eb211c80319c");
        assert!(span.is_remote() && span.is_sampled());
        assert_eq!(span.trace_state().header(), "a=1");
        for bad in ["", "garbage", "00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01", "zz-0af7-b7ad-01"] {
            assert!(remote_context(&headers(bad)).is_none(), "{:?}", bad);
        }
        assert!(remote_context(&http::HeaderMap::new()).is_none());
    }
}
