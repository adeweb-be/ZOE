//! The traces of `zoe serve`: OpenTelemetry spans, exported over OTLP.
//!
//! The core and the server emit `tracing` spans, never OpenTelemetry
//! types. When `ZOE_OTLP_ENDPOINT` is set, `zoe serve` adds a layer that
//! makes OpenTelemetry spans of the spans of the zoe crates, and a batch
//! exporter that sends them over OTLP. The protocol comes from
//! `OTEL_EXPORTER_OTLP_PROTOCOL`: `grpc`, `http/protobuf` (the default) or
//! `http/json`. The exporter sets the endpoint and the protocol itself, so
//! the signal variables (`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`,
//! `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL`), which a host process or a CI can
//! set for its own collector, change neither. Without `ZOE_OTLP_ENDPOINT`,
//! no exporter starts.
//!
//! Each call of a session runs in a span `request`, which the traces name
//! after the method of the call (`otel.name`). `request` puts the name of
//! the storage in the OpenTelemetry context of that span. Each span under it
//! gets that context, the spans of the core too. `StorageAttribute` then
//! gives each span the attribute `zoe.storage`. So the core never names a
//! storage. A call with a valid W3C traceparent (the extension
//! `trace_context`) joins the trace of its client: its span gets the trace
//! id of the traceparent and the span of the client as its parent. Any
//! other call is the root of its trace.
//!
//! The log lines show the events and the span `connection` only. The
//! traces take the spans of the zoe crates, except `connection`, and no
//! event.

use std::sync::Arc;
use std::time::Duration;

use opentelemetry::propagation::{Extractor, TextMapPropagator as _};
use opentelemetry::trace::{Span as _, TracerProvider as _};
use opentelemetry::{Context, KeyValue};
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracerProvider, Span, SpanData, SpanProcessor};
use tracing::level_filters::LevelFilter;
use tracing::subscriber::Interest;
use tracing::{Level, Metadata, Subscriber};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::{self, Filter, Layer};
use tracing_subscriber::registry::LookupSpan;

/// The variable that names the OTLP endpoint. Without it, no exporter
/// starts.
pub(crate) const ENDPOINT: &str = "ZOE_OTLP_ENDPOINT";

/// The span of each call of a session in the log lines. The traces leave
/// it out.
pub(crate) const CONNECTION: &str = "connection";

/// The attribute that names the storage of a span.
const STORAGE: &str = "zoe.storage";

/// The exporter of the traces. Its drop flushes the spans that wait, then
/// stops the exporter.
pub(crate) struct Telemetry {
    provider: SdkTracerProvider,
    /// The runtime of the gRPC client. It drops after the flush.
    _runtime: Option<tokio::runtime::Runtime>,
}

impl Telemetry {
    /// Start the exporter that `ZOE_OTLP_ENDPOINT` and
    /// `OTEL_EXPORTER_OTLP_PROTOCOL` configure. Return `None` without `ZOE_OTLP_ENDPOINT`.
    pub(crate) fn start() -> Result<Option<Self>, String> {
        let Some(endpoint) = std::env::var(ENDPOINT).ok().filter(|e| !e.is_empty()) else {
            return Ok(None);
        };
        // An empty variable counts as no variable.
        let name = std::env::var(opentelemetry_otlp::OTEL_EXPORTER_OTLP_PROTOCOL)
            .ok()
            .filter(|p| !p.is_empty());
        let protocol = protocol(name.as_deref())?;
        let failed = |error: opentelemetry_otlp::ExporterBuildError| format!("{ENDPOINT}: {error}");
        let (exporter, runtime) = if protocol == Protocol::Grpc {
            // The gRPC client needs a tokio runtime. The server runtime
            // stops before the flush, so the client gets its own.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("zoe-otlp")
                .enable_all()
                .build()
                .map_err(|error| format!("the runtime of the OTLP exporter: {error}"))?;
            let exporter = {
                let _entered = runtime.enter();
                SpanExporter::builder()
                    .with_tonic()
                    .with_endpoint(endpoint)
                    .build()
                    .map_err(failed)?
            };
            (exporter, Some(runtime))
        } else {
            let exporter = SpanExporter::builder()
                .with_http()
                .with_protocol(protocol)
                .with_endpoint(traces_url(&endpoint))
                .build()
                .map_err(failed)?;
            (exporter, None)
        };
        let mut resource = Resource::builder();
        if std::env::var_os("OTEL_SERVICE_NAME").is_none() {
            resource = resource.with_service_name("zoe");
        }
        let provider = SdkTracerProvider::builder()
            .with_resource(resource.build())
            .with_span_processor(StorageAttribute)
            .with_batch_exporter(exporter)
            .build();
        Ok(Some(Self {
            provider,
            _runtime: runtime,
        }))
    }

    /// The layer that makes OpenTelemetry spans of the spans that
    /// `is_traced` selects.
    pub(crate) fn layer<S>(&self) -> impl Layer<S> + use<S>
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        tracing_opentelemetry::layer()
            .with_tracer(self.provider.tracer("zoe"))
            .with_filter(Select(is_traced))
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Err(error) = self.provider.shutdown() {
            tracing::warn!("the traces did not flush: {error}");
        }
    }
}

/// Parse the OTLP protocol `name`. No name gives `http/protobuf`.
fn protocol(name: Option<&str>) -> Result<Protocol, String> {
    match name.unwrap_or("http/protobuf") {
        "grpc" => Ok(Protocol::Grpc),
        "http/protobuf" => Ok(Protocol::HttpBinary),
        "http/json" => Ok(Protocol::HttpJson),
        other => Err(format!(
            "the OTLP protocol {other:?} is not known: give grpc, http/protobuf or http/json"
        )),
    }
}

/// The URL of the traces under `endpoint`, as OTLP over HTTP adds it to
/// `OTEL_EXPORTER_OTLP_ENDPOINT`.
fn traces_url(endpoint: &str) -> String {
    format!("{}/v1/traces", endpoint.trim_end_matches('/'))
}

/// Return the span of a call of a session on `storage`, named after
/// `method` in the traces. With a valid W3C `traceparent`, the span joins
/// that trace, with the span of the traceparent as its parent. Otherwise,
/// the span is the root of its trace. The span is disabled when no layer
/// takes it.
pub(crate) fn request(method: &str, storage: &str, traceparent: Option<&str>) -> tracing::Span {
    let span = tracing::info_span!("request", otel.name = method);
    if !span.is_disabled() {
        // An invalid traceparent gives the empty context back.
        let parent = match traceparent {
            Some(traceparent) => TraceContextPropagator::new()
                .extract_with_context(&Context::new(), &Traceparent(traceparent)),
            None => Context::new(),
        };
        // The span did not start yet, so its parent context can change.
        let named = parent.with_value(StorageName(Arc::from(storage)));
        let _ = span.set_parent(named);
    }
    span
}

/// The traceparent of a call, as the W3C propagator reads it. It has no
/// `tracestate`.
struct Traceparent<'a>(&'a str);

impl Extractor for Traceparent<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        (key == "traceparent").then_some(self.0)
    }

    fn keys(&self) -> Vec<&str> {
        vec!["traceparent"]
    }
}

/// The name of the storage in an OpenTelemetry context.
#[derive(Debug)]
struct StorageName(Arc<str>);

/// The processor that gives each span the attribute `zoe.storage`, from
/// the storage name in its parent context.
#[derive(Debug)]
struct StorageAttribute;

impl SpanProcessor for StorageAttribute {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        if let Some(StorageName(name)) = cx.get::<StorageName>() {
            span.set_attribute(KeyValue::new(STORAGE, Arc::clone(name)));
        }
    }

    fn on_end(&self, _span: SpanData) {}

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

/// Tell if the log lines show `meta`: each event, and the span
/// `connection`.
pub(crate) fn is_logged(meta: &Metadata<'_>) -> bool {
    meta.is_event() || meta.name() == CONNECTION
}

/// Tell if the traces take `meta`: each span of the zoe crates, except
/// `connection`. A span of another crate, such as the HTTP client of the
/// exporter, stays out, so an export makes no new span.
pub(crate) fn is_traced(meta: &Metadata<'_>) -> bool {
    meta.is_span() && meta.name() != CONNECTION && meta.target().starts_with("zoe_")
}

/// The filter of a layer: the spans and events from the level INFO on
/// that its function selects. The metadata of a callsite does not change,
/// so the decision of a callsite holds for good.
pub(crate) struct Select(pub(crate) fn(&Metadata<'_>) -> bool);

impl Select {
    fn selects(&self, meta: &Metadata<'_>) -> bool {
        *meta.level() <= Level::INFO && (self.0)(meta)
    }
}

impl<S> Filter<S> for Select {
    fn enabled(&self, meta: &Metadata<'_>, _: &layer::Context<'_, S>) -> bool {
        self.selects(meta)
    }

    fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> Interest {
        if self.selects(meta) {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::INFO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_protocol_is_http_protobuf_by_default_and_takes_the_three_names() {
        assert_eq!(protocol(None), Ok(Protocol::HttpBinary));
        assert_eq!(protocol(Some("grpc")), Ok(Protocol::Grpc));
        assert_eq!(protocol(Some("http/protobuf")), Ok(Protocol::HttpBinary));
        assert_eq!(protocol(Some("http/json")), Ok(Protocol::HttpJson));
        let error = protocol(Some("http")).unwrap_err();
        assert!(error.contains("\"http\""), "{error}");
    }

    #[test]
    fn the_traces_url_is_under_the_endpoint() {
        assert_eq!(
            traces_url("http://collector:4318"),
            "http://collector:4318/v1/traces"
        );
        assert_eq!(
            traces_url("http://collector:4318/"),
            "http://collector:4318/v1/traces"
        );
    }

    #[test]
    fn a_request_span_without_the_traces_layer_is_disabled() {
        use tracing_subscriber::layer::SubscriberExt;

        // The subscriber of the log lines alone, as without
        // `ZOE_OTLP_ENDPOINT`. The tests share the callsites, so the test
        // needs a subscriber of its own: with none, a callsite that
        // another test enabled gives a span that is not disabled.
        let logs = tracing_subscriber::fmt::layer()
            .with_writer(std::io::sink)
            .with_filter(Select(is_logged));
        let subscriber = tracing_subscriber::registry().with(logs);
        tracing::subscriber::with_default(subscriber, || {
            assert!(request("loadBefore", "1", None).is_disabled());
            assert!(!tracing::info_span!("connection").is_disabled());
        });
    }

    /// A processor that keeps each span that ends.
    #[derive(Clone, Debug, Default)]
    struct Kept(Arc<std::sync::Mutex<Vec<SpanData>>>);

    impl SpanProcessor for Kept {
        fn on_start(&self, _span: &mut Span, _cx: &Context) {}

        fn on_end(&self, span: SpanData) {
            self.0.lock().unwrap().push(span);
        }

        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }

        fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
            Ok(())
        }
    }

    #[test]
    fn a_span_under_a_request_has_its_storage_and_the_request_as_parent() {
        use tracing_subscriber::layer::SubscriberExt;

        let kept = Kept::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(StorageAttribute)
            .with_span_processor(kept.clone())
            .build();
        let telemetry = Telemetry {
            provider,
            _runtime: None,
        };
        let subscriber = tracing_subscriber::registry().with(telemetry.layer());
        tracing::subscriber::with_default(subscriber, || {
            let _connection = tracing::info_span!("connection", client = "c").entered();
            let _request = request("vote", "main", None).entered();
            let _wait = tracing::info_span!("lock_wait", oid = 7u64).entered();
            tracing::warn!("an event is not a span");
        });
        let spans = kept.0.lock().unwrap();
        let names: Vec<&str> = spans.iter().map(|span| span.name.as_ref()).collect();
        // The connection span and the event stay out of the traces.
        assert_eq!(names, ["lock_wait", "vote"]);
        for span in spans.iter() {
            let storage = span
                .attributes
                .iter()
                .find(|kv| kv.key.as_str() == STORAGE)
                .map(|kv| kv.value.as_str().into_owned());
            assert_eq!(storage.as_deref(), Some("main"), "{}", span.name);
            assert!(span.events.is_empty(), "{}", span.name);
        }
        let (wait, vote) = (&spans[0], &spans[1]);
        assert_eq!(wait.parent_span_id, vote.span_context.span_id());
        assert_eq!(wait.span_context.trace_id(), vote.span_context.trace_id());
        assert_eq!(vote.parent_span_id, opentelemetry::trace::SpanId::INVALID);
    }

    /// Run a call `request` with `traceparent` under a provider that keeps
    /// the spans, and return them.
    fn traced(traceparent: Option<&str>) -> Vec<SpanData> {
        use tracing_subscriber::layer::SubscriberExt;

        let kept = Kept::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(StorageAttribute)
            .with_span_processor(kept.clone())
            .build();
        let telemetry = Telemetry {
            provider,
            _runtime: None,
        };
        let subscriber = tracing_subscriber::registry().with(telemetry.layer());
        tracing::subscriber::with_default(subscriber, || {
            let _request = request("loadBefore", "main", traceparent).entered();
            let _wait = tracing::info_span!("lock_wait").entered();
        });
        kept.0.lock().unwrap().clone()
    }

    #[test]
    fn a_request_with_a_traceparent_joins_the_trace_of_the_client() {
        use opentelemetry::trace::{SpanId, TraceId};

        let spans = traced(Some(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        ));
        let (wait, load) = (&spans[0], &spans[1]);
        let trace = TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap();
        assert_eq!(load.span_context.trace_id(), trace);
        assert_eq!(
            load.parent_span_id,
            SpanId::from_hex("00f067aa0ba902b7").unwrap()
        );
        assert!(load.parent_span_is_remote);
        // The spans under the request stay in the trace, with their storage.
        assert_eq!(wait.span_context.trace_id(), trace);
        assert_eq!(wait.parent_span_id, load.span_context.span_id());
        for span in &spans {
            assert!(
                span.attributes
                    .iter()
                    .any(|kv| kv.key.as_str() == STORAGE && kv.value.as_str() == "main"),
                "{}",
                span.name
            );
        }
    }

    #[test]
    fn a_request_with_an_invalid_traceparent_is_the_root_of_its_trace() {
        use opentelemetry::trace::SpanId;

        for traceparent in [
            "",
            "not a traceparent",
            // A trace id of zeros is not valid.
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            // W3C asks for lower case.
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
        ] {
            let spans = traced(Some(traceparent));
            assert_eq!(spans.len(), 2, "{traceparent:?}");
            assert_eq!(spans[1].parent_span_id, SpanId::INVALID, "{traceparent:?}");
        }
    }
}
