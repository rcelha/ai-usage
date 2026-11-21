use std::{
    collections::HashSet,
    convert::Infallible,
    io::{self, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use axum::{
    Router,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use chrono::{DateTime, NaiveDate, Utc};
use clap::ValueEnum;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpListener,
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tokio_stream::{StreamExt, wrappers::WatchStream};

use crate::{Paths, Provider, Report, Session, collect, pricing::Pricing};

const WEB_HTML: &str = include_str!("web.html");

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub paths: Paths,
    pub pricing: Pricing,
    pub pricing_path: Option<PathBuf>,
    pub providers: Vec<Provider>,
    pub since: Option<NaiveDate>,
    pub limit: Option<usize>,
    pub title_width: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FilterDefaults {
    pub providers: Vec<Provider>,
    pub since: Option<NaiveDate>,
    pub limit: Option<usize>,
    pub title_width: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct LivePayload {
    pub revision: u64,
    pub updated_at: DateTime<Utc>,
    pub matched: usize,
    pub defaults: FilterDefaults,
    #[serde(flatten)]
    pub report: Report,
    pub all_sessions: Vec<Session>,
}

#[derive(Debug, Clone)]
struct Snapshot {
    base_report: Report,
    payload_json: Arc<String>,
}

#[derive(Clone)]
struct AppState {
    config: Arc<ServerConfig>,
    rx: watch::Receiver<Arc<Snapshot>>,
}

#[derive(Debug, Default, Deserialize)]
struct ReportQuery {
    provider: Option<String>,
    since: Option<String>,
    limit: Option<usize>,
}

pub fn resolve_bind_addr(serve: Option<&str>, port_override: Option<u16>) -> Result<SocketAddr> {
    let raw = serve.unwrap_or("127.0.0.1:8080").trim();
    let mut addr = if raw.is_empty() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)
    } else if let Ok(port) = raw.parse::<u16>() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    } else if let Some(rest) = raw.strip_prefix(':') {
        let port: u16 = rest
            .parse()
            .with_context(|| format!("invalid port in '{raw}'"))?;
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    } else if let Ok(sock) = raw.parse::<SocketAddr>() {
        sock
    } else {
        let mut resolved: Vec<SocketAddr> = raw
            .to_socket_addrs()
            .with_context(|| format!("invalid bind address '{raw}'"))?
            .collect();
        resolved.sort_by_key(|a| !a.is_ipv4());
        resolved
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("could not resolve bind address '{raw}'"))?
    };
    if let Some(p) = port_override {
        addr.set_port(p);
    }
    Ok(addr)
}

fn collect_base(config: &ServerConfig) -> Report {
    let pricing = config
        .pricing_path
        .as_deref()
        .and_then(|p| Pricing::load(Some(p)).ok())
        .unwrap_or_else(|| config.pricing.clone());
    collect(&config.paths, &config.providers, &pricing)
}

fn build_snapshot(revision: u64, base_report: Report, config: &ServerConfig) -> Snapshot {
    let updated_at = Utc::now();
    let all_sessions = base_report.sessions.clone();
    let mut filtered = base_report.clone();
    let matched = filtered.apply_filters(config.since, config.limit);
    let payload = LivePayload {
        revision,
        updated_at,
        matched,
        defaults: FilterDefaults {
            providers: config.providers.clone(),
            since: config.since,
            limit: config.limit,
            title_width: config.title_width,
        },
        report: filtered,
        all_sessions,
    };
    let json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned());
    Snapshot {
        base_report,
        payload_json: Arc::new(json),
    }
}

fn watch_targets(config: &ServerConfig) -> Vec<PathBuf> {
    let mut targets = Vec::new();
    for (provider, path) in [
        (Provider::Opencode, &config.paths.opencode),
        (Provider::Claude, &config.paths.claude),
        (Provider::Codex, &config.paths.codex),
    ] {
        if config.providers.is_empty() || config.providers.contains(&provider) {
            targets.push(path.clone());
        }
    }
    if let Some(ref pricing) = config.pricing_path {
        targets.push(pricing.clone());
    }
    targets
}

fn sync_watches(
    watcher: &mut RecommendedWatcher,
    watched_recursive: &mut HashSet<PathBuf>,
    watched_parents: &mut HashSet<PathBuf>,
    config: &ServerConfig,
) {
    for target in watch_targets(config) {
        if target.exists() {
            if !watched_recursive.contains(&target)
                && watcher.watch(&target, RecursiveMode::Recursive).is_ok()
            {
                watched_recursive.insert(target);
            }
        } else if let Some(parent) = target.parent()
            && parent.exists()
            && !watched_parents.contains(parent)
            && watcher.watch(parent, RecursiveMode::NonRecursive).is_ok()
        {
            watched_parents.insert(parent.to_path_buf());
        }
    }
}

pub struct RunningServer {
    local_addr: SocketAddr,
    shutdown_tx: watch::Sender<bool>,
    server_task: Option<JoinHandle<Result<()>>>,
    watcher_task: JoinHandle<()>,
}

impl RunningServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.shutdown_tx.send(true);
        self.watcher_task.abort();
        if let Some(task) = self.server_task.take() {
            task.await.context("joining server task")??;
        }
        Ok(())
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        self.watcher_task.abort();
        if let Some(ref task) = self.server_task {
            task.abort();
        }
    }
}

pub async fn bind_and_start(addr: SocketAddr, config: ServerConfig) -> Result<RunningServer> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding webserver to {addr}"))?;
    let local_addr = listener
        .local_addr()
        .context("reading bound local address")?;

    let cfg_for_init = config.clone();
    let initial_base = tokio::task::spawn_blocking(move || collect_base(&cfg_for_init))
        .await
        .context("collecting initial usage report")?;
    let initial_snapshot = Arc::new(build_snapshot(1, initial_base.clone(), &config));

    let (watch_tx, watch_rx) = watch::channel(initial_snapshot);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let watcher_config = config.clone();
    let mut watcher_shutdown = shutdown_rx.clone();
    let watcher_task = tokio::spawn(async move {
        let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<()>();
        let mut watcher: Option<RecommendedWatcher> =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res
                    && !matches!(event.kind, EventKind::Access(_))
                {
                    let _ = notify_tx.send(());
                }
            })
            .ok();

        let mut watched_recursive = HashSet::new();
        let mut watched_parents = HashSet::new();
        if let Some(ref mut w) = watcher {
            sync_watches(
                w,
                &mut watched_recursive,
                &mut watched_parents,
                &watcher_config,
            );
        }

        let mut current_base = initial_base;
        let mut revision = 1u64;
        let mut poll = tokio::time::interval(Duration::from_millis(750));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                Some(()) = notify_rx.recv() => {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    while notify_rx.try_recv().is_ok() {}
                }
                _ = poll.tick() => {}
                _ = watcher_shutdown.changed() => {
                    break;
                }
            }

            if *watcher_shutdown.borrow() {
                break;
            }

            if let Some(ref mut w) = watcher {
                sync_watches(
                    w,
                    &mut watched_recursive,
                    &mut watched_parents,
                    &watcher_config,
                );
            }

            let cfg = watcher_config.clone();
            if let Ok(next_base) = tokio::task::spawn_blocking(move || collect_base(&cfg)).await
                && next_base != current_base
            {
                revision += 1;
                current_base = next_base.clone();
                let snap = Arc::new(build_snapshot(revision, next_base, &watcher_config));
                let _ = watch_tx.send(snap);
            }
        }
    });

    let app_state = AppState {
        config: Arc::new(config),
        rx: watch_rx,
    };

    let router = Router::new()
        .route("/", get(index_handler))
        .route("/api/report", get(report_handler))
        .route("/api/events", get(sse_handler))
        .with_state(app_state);

    let mut server_shutdown = shutdown_rx;
    let server_task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = server_shutdown.changed().await;
            })
            .await
            .context("running HTTP server")
    });

    Ok(RunningServer {
        local_addr,
        shutdown_tx,
        server_task: Some(server_task),
        watcher_task,
    })
}

async fn index_handler() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        WEB_HTML,
    )
}

async fn report_handler(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
) -> Response {
    let snap = state.rx.borrow().clone();

    if query.provider.is_none() && query.since.is_none() && query.limit.is_none() {
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/json; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            (*snap.payload_json).clone(),
        )
            .into_response();
    }

    let mut providers_filter = Vec::new();
    if let Some(ref raw_providers) = query.provider {
        for part in raw_providers
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match Provider::from_str(part, true) {
                Ok(p) => providers_filter.push(p),
                Err(_) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
                        serde_json::json!({"error": format!("unknown provider '{part}'")})
                            .to_string(),
                    )
                        .into_response();
                }
            }
        }
    }

    let since = match query
        .since
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => match raw.parse::<NaiveDate>() {
            Ok(d) => Some(d),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
                    serde_json::json!({"error": format!("invalid date '{raw}', expected YYYY-MM-DD")})
                        .to_string(),
                )
                    .into_response();
            }
        },
        None => state.config.since,
    };

    let limit = query.limit.or(state.config.limit);

    let mut report = snap.base_report.clone();
    if !providers_filter.is_empty() {
        report
            .sessions
            .retain(|s| providers_filter.contains(&s.provider));
    }
    let matched = report.apply_filters(since, limit);

    #[derive(Serialize)]
    struct FilteredResponse {
        matched: usize,
        #[serde(flatten)]
        report: Report,
    }

    let body = serde_json::to_string(&FilteredResponse { matched, report })
        .unwrap_or_else(|_| "{}".to_owned());

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn sse_handler(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = std::result::Result<Event, Infallible>>> {
    let stream = WatchStream::new(state.rx.clone())
        .map(|snap| Ok(Event::default().data((*snap.payload_json).clone())));
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

pub fn run_blocking(addr: SocketAddr, config: ServerConfig) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("initializing async runtime")?;

    rt.block_on(async move {
        let server = bind_and_start(addr, config).await?;
        let bound = server.local_addr();
        {
            let mut out = io::stdout().lock();
            let _ = writeln!(out, "Listening on http://{bound}");
            let _ = out.flush();
        }

        wait_for_shutdown_signal().await;
        server.shutdown().await
    })
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                if let Some(ref mut s) = sigterm {
                    s.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
