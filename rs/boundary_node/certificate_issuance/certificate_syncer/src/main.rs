use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context as AnyhowContext, Error};
use async_trait::async_trait;
use axum::{
    body::Body,
    handler::Handler,
    http::{Response, StatusCode},
    routing::get,
    Extension, Router, Server,
};
use clap::Parser;
use futures::future::TryFutureExt;
use hyper::Uri;
use hyper_rustls::HttpsConnectorBuilder;
use import::Import;
use nix::sys::signal::Signal;
use opentelemetry::{
    global,
    sdk::{
        export::metrics::aggregation,
        metrics::{controllers, processors, selectors},
        Resource,
    },
    Context, KeyValue,
};
use opentelemetry_prometheus::{ExporterBuilder, PrometheusExporter};
use persist::Persist;
use prometheus::{Encoder as PrometheusEncoder, TextEncoder};
use tokio::task;
use tracing::info;

use crate::{
    http::HyperClient,
    import::CertificatesImporter,
    metrics::{MetricParams, WithMetrics},
    persist::{Persister, WithDedup, WithEmpty},
    reload::{Reloader, WithReload},
    render::Renderer,
    verify::{Parser as CertificateParser, Verifier, WithVerify},
};

mod http;
mod import;
mod metrics;
mod persist;
mod reload;
mod render;
mod verify;

const SERVICE_NAME: &str = "certificate-syncer";

const SECOND: Duration = Duration::from_secs(1);

#[derive(Parser)]
#[command(name = SERVICE_NAME)]
struct Cli {
    #[clap(long, default_value = "/var/run/nginx.pid")]
    pid_path: PathBuf,

    #[clap(long, default_value = "http://127.0.0.1:3000/certificates")]
    certificates_exporter_uri: Uri,

    #[clap(long, default_value = "certs")]
    local_certificates_path: PathBuf,

    #[clap(long, default_value = "servers.conf")]
    local_configuration_path: PathBuf,

    #[clap(long, default_value = "servers.conf.tmpl")]
    configuration_template_path: PathBuf,

    #[clap(long, default_value = "mappings.js")]
    domain_mappings_path: PathBuf,

    #[arg(long, default_value = "127.0.0.1:9090")]
    metrics_addr: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let cli = Cli::parse();

    // Logging
    let subscriber = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .context("failed to set global subscriber")?;

    // Metrics
    let exporter = ExporterBuilder::new(
        controllers::basic(
            processors::factory(
                selectors::simple::histogram([]),
                aggregation::cumulative_temporality_selector(),
            )
            .with_memory(true),
        )
        .with_resource(Resource::new(vec![KeyValue::new("service", SERVICE_NAME)]))
        .build(),
    )
    .init();

    let meter = global::meter(SERVICE_NAME);

    let metrics_handler = metrics_handler.layer(Extension(MetricsHandlerArgs { exporter }));
    let metrics_router = Router::new().route("/metrics", get(metrics_handler));

    // HTTP
    let http_client = hyper::Client::builder().build(
        HttpsConnectorBuilder::new()
            .with_native_roots()
            .https_or_http()
            .enable_http1()
            .build(),
    );
    let http_client = HyperClient::new(http_client);
    let http_client = WithMetrics(
        http_client,
        MetricParams::new(&meter, SERVICE_NAME, "http_request"),
    );
    let http_client = Arc::new(http_client);

    // Certificates
    let importer = CertificatesImporter::new(http_client, cli.certificates_exporter_uri);
    let importer = WithVerify(importer, Verifier(CertificateParser));
    let importer = WithMetrics(importer, MetricParams::new(&meter, SERVICE_NAME, "import"));
    let importer = Arc::new(importer);

    // Service Reload
    let reloader = Reloader::new(cli.pid_path, Signal::SIGHUP);
    let reloader = WithMetrics(reloader, MetricParams::new(&meter, SERVICE_NAME, "reload"));

    // Persistence
    let configuration_template = std::fs::read_to_string(&cli.configuration_template_path)
        .context("failed to read configuration template")?;

    let renderer = Renderer::new(&configuration_template);
    let renderer = WithMetrics(renderer, MetricParams::new(&meter, SERVICE_NAME, "render"));
    let renderer = Arc::new(renderer);

    let persister = Persister::new(
        renderer,
        cli.local_certificates_path,
        cli.local_configuration_path,
        cli.domain_mappings_path,
    );
    let persister = WithReload(persister, reloader);
    let persister = WithDedup(persister, Arc::new(RwLock::new(None)));
    let persister = WithEmpty(persister);
    let persister = WithMetrics(
        persister,
        MetricParams::new(&meter, SERVICE_NAME, "persist"),
    );
    let persister = Arc::new(persister);

    // Runner
    let runner = Runner::new(importer, persister);
    let runner = WithMetrics(runner, MetricParams::new(&meter, SERVICE_NAME, "run"));
    let runner = WithThrottle(runner, ThrottleParams::new(10 * SECOND));
    let mut runner = runner;

    // Service
    info!(
        msg = format!("starting {SERVICE_NAME}").as_str(),
        metrics_addr = cli.metrics_addr.to_string().as_str(),
    );

    let _ = tokio::try_join!(
        task::spawn(async move {
            loop {
                let _ = runner.run().await;
            }
        }),
        task::spawn(
            Server::bind(&cli.metrics_addr)
                .serve(metrics_router.into_make_service())
                .map_err(|err| anyhow!("server failed: {:?}", err))
        ),
    )
    .context(format!("{SERVICE_NAME} failed to run"))?;

    Ok(())
}

#[derive(Clone)]
struct MetricsHandlerArgs {
    exporter: PrometheusExporter,
}

async fn metrics_handler(
    Extension(MetricsHandlerArgs { exporter }): Extension<MetricsHandlerArgs>,
) -> Response<Body> {
    let metric_families = exporter.registry().gather();

    let encoder = TextEncoder::new();

    let mut metrics_text = Vec::new();
    if encoder.encode(&metric_families, &mut metrics_text).is_err() {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body("Internal Server Error".into())
            .unwrap();
    };

    Response::builder()
        .status(200)
        .body(metrics_text.into())
        .unwrap()
}

struct ThrottleParams {
    throttle_duration: Duration,
    next_time: Option<Instant>,
}

impl ThrottleParams {
    fn new(throttle_duration: Duration) -> Self {
        Self {
            throttle_duration,
            next_time: None,
        }
    }
}

struct WithThrottle<T>(T, ThrottleParams);

#[async_trait]
trait Run: Send + Sync {
    async fn run(&mut self) -> Result<(), Error>;
}

struct Runner {
    importer: Arc<dyn Import>,
    persister: Arc<dyn Persist>,
}

impl Runner {
    fn new(importer: Arc<dyn Import>, persister: Arc<dyn Persist>) -> Self {
        Self {
            importer,
            persister,
        }
    }
}

#[async_trait]
impl Run for Runner {
    async fn run(&mut self) -> Result<(), Error> {
        let pkgs = self
            .importer
            .import()
            .await
            .context("failed to import certificates")?;

        self.persister
            .persist(&pkgs)
            .await
            .context("failed to persist certificates")?;

        Ok(())
    }
}

#[async_trait]
impl<T: Run + Send + Sync> Run for WithThrottle<T> {
    async fn run(&mut self) -> Result<(), Error> {
        let current_time = Instant::now();
        let next_time = self.1.next_time.unwrap_or(current_time);

        if next_time > current_time {
            tokio::time::sleep(next_time - current_time).await;
        }
        self.1.next_time = Some(Instant::now() + self.1.throttle_duration);

        self.0.run().await
    }
}

#[async_trait]
impl<T: Run> Run for WithMetrics<T> {
    async fn run(&mut self) -> Result<(), Error> {
        let start_time = Instant::now();

        let out = self.0.run().await;

        let status = if out.is_ok() { "ok" } else { "fail" };
        let duration = start_time.elapsed().as_secs_f64();

        let labels = &[KeyValue::new("status", status)];

        let MetricParams {
            action,
            counter,
            recorder,
        } = &self.1;

        let cx = Context::current();

        counter.add(&cx, 1, labels);
        recorder.record(&cx, duration, labels);

        info!(action = action.as_str(), status, duration, error = ?out.as_ref().err());

        out
    }
}
