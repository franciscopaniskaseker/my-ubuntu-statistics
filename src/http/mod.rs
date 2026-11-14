//! HTTP server: embedded page, JSON API and the SSE live stream.

pub mod assets;
pub mod guard;

use crate::backup;
use crate::config::Config;
use crate::hub::Hub;
use crate::settings::{COLLECTORS, Settings};
use crate::storage::{Storage, query};
use crate::util::sysfs::Roots;
use crate::util::time::{now_ms, now_s};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{Semaphore, watch};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

pub const MAX_SSE_CLIENTS: usize = 32;
pub const PROC_SLOTS: usize = 4;
const PROC_TIMEOUT: Duration = Duration::from_secs(3);

pub struct AppState {
    pub hub: Arc<Hub>,
    pub storage: Option<Arc<Storage>>,
    pub storage_error: Option<String>,
    pub settings_tx: watch::Sender<Arc<Settings>>,
    pub backups: Arc<backup::Manager>,
    pub config: Config,
    pub host: Value,
    pub notes: Vec<String>,
    pub sse_clients: AtomicUsize,
    pub roots: Roots,
    /// Bounds concurrent on-demand /proc reads (a read can block while a
    /// process is stuck in the kernel).
    pub proc_slots: Arc<Semaphore>,
}

type St = Arc<AppState>;

pub fn router(state: St) -> Router {
    let policy = Arc::new(guard::HostPolicy::new(
        state.config.listen,
        &state.config.allowed_hosts,
    ));
    Router::new()
        .route("/", get(index))
        .route("/assets/{*path}", get(asset))
        .route("/favicon.ico", get(favicon))
        .route("/api/health", get(health))
        .route("/api/meta", get(meta))
        .route("/api/snapshot", get(snapshot))
        .route("/api/stream", get(stream))
        .route("/api/live", get(live))
        .route("/api/history", get(history))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/status", get(status))
        .route("/api/processes", get(processes))
        .route("/api/process", get(process))
        .route("/api/backups", get(list_backups).post(create_backup))
        .layer(axum::middleware::from_fn_with_state(policy, guard::guard))
        .with_state(state)
}

fn serve_asset(body: std::borrow::Cow<'static, [u8]>, mime: &'static str) -> Response {
    let mut r = match body {
        std::borrow::Cow::Borrowed(b) => b.into_response(),
        std::borrow::Cow::Owned(b) => b.into_response(),
    };
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    r
}

async fn index() -> Response {
    match assets::get("index.html") {
        Some((body, mime)) => {
            let mut r = serve_asset(body, mime);
            r.headers_mut().insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(
                    "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'",
                ),
            );
            r
        }
        None => (StatusCode::INTERNAL_SERVER_ERROR, "index missing").into_response(),
    }
}

async fn asset(Path(path): Path<String>) -> Response {
    match assets::get(&path) {
        Some((body, mime)) => serve_asset(body, mime),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn favicon() -> Response {
    match assets::get("favicon.svg") {
        Some((body, mime)) => serve_asset(body, mime),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn health(State(st): State<St>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "instance": st.hub.instance,
        "uptime_s": (now_ms() - st.hub.started_ms) / 1000,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn meta(State(st): State<St>) -> Json<Value> {
    let (version, cat) = st.hub.catalog();
    let collectors: Vec<Value> = COLLECTORS
        .iter()
        .map(|(n, d, l)| json!({"name": n, "label": l, "default_interval_ms": d}))
        .collect();
    Json(json!({
        "instance": st.hub.instance,
        "version": env!("CARGO_PKG_VERSION"),
        "host": st.host,
        "catalog_version": version,
        "series": cat,
        "collectors": collectors,
        "history_available": st.storage.is_some(),
        "now_ms": now_ms(),
    }))
}

async fn snapshot(State(st): State<St>) -> Json<Value> {
    Json(st.hub.snapshot())
}

struct ClientGuard(St);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.sse_clients.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn stream(State(st): State<St>) -> Response {
    if st.sse_clients.fetch_add(1, Ordering::Relaxed) >= MAX_SSE_CLIENTS {
        st.sse_clients.fetch_sub(1, Ordering::Relaxed);
        return (StatusCode::SERVICE_UNAVAILABLE, "too many live clients").into_response();
    }
    let guard = ClientGuard(st.clone());
    let hello = Event::default().event("hello").data(
        json!({"instance": st.hub.instance, "catalog_version": st.hub.catalog().0}).to_string(),
    );
    let rx = st.hub.subscribe();
    let events = BroadcastStream::new(rx).map(move |r| {
        let _keep = &guard;
        let ev = match r {
            Ok(m) => Event::default().event(m.event).data(m.data.as_str()),
            Err(BroadcastStreamRecvError::Lagged(n)) => {
                Event::default().event("resync").data(n.to_string())
            }
        };
        Ok::<_, Infallible>(ev)
    });
    let s = tokio_stream::once(Ok::<_, Infallible>(hello)).chain(events);
    Sse::new(s)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

fn split_keys(s: &str) -> Vec<String> {
    s.split(',')
        .map(|k| k.trim())
        .filter(|k| !k.is_empty() && k.len() < 200)
        .take(400)
        .map(String::from)
        .collect()
}

#[derive(Deserialize)]
struct LiveQ {
    keys: String,
    since_ms: Option<i64>,
}

async fn live(State(st): State<St>, Query(q): Query<LiveQ>) -> Json<Value> {
    let keys = split_keys(&q.keys);
    let since = q.since_ms.unwrap_or(now_ms() - crate::hub::RING_WINDOW_MS);
    Json(json!({"now_ms": now_ms(), "series": st.hub.live(&keys, since)}))
}

#[derive(Deserialize)]
struct HistQ {
    keys: String,
    from: Option<i64>,
    to: Option<i64>,
    points: Option<usize>,
}

async fn history(State(st): State<St>, Query(q): Query<HistQ>) -> Response {
    let keys = split_keys(&q.keys);
    let now = now_s();
    let to = q.to.unwrap_or(now).min(now + 60);
    let from = q.from.unwrap_or(to - 3600).max(0);
    if from >= to {
        return (StatusCode::BAD_REQUEST, "from must be before to").into_response();
    }
    let points = q.points.unwrap_or(600).clamp(10, 4000);
    let settings = st.settings_tx.borrow().clone();
    let db = st.storage.as_ref().map(|s| s.path.clone());
    let hub = st.hub.clone();
    let res = tokio::task::spawn_blocking(move || {
        let req = query::Request {
            keys: &keys,
            from,
            to,
            points,
            now,
            history: &settings.history,
        };
        let ring = |k: &str, since: i64| hub.ring_points(k, since);
        let gap = |k: &str| {
            hub.desc(k)
                .map(|d| {
                    let iv = settings.collector(d.collector).interval_ms;
                    d.period_ms.unwrap_or(0).max(iv) as i64 / 1000
                })
                .unwrap_or(1)
                .max(1)
        };
        query::history(db.as_deref(), &req, &ring, &gap)
    })
    .await;
    match res {
        Ok(Ok((step, series))) => {
            Json(json!({"from": from, "to": to, "step": step, "series": series})).into_response()
        }
        Ok(Err(e)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("history unavailable: {e:#}"),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("history task failed: {e}"),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct ProcQ {
    app: Option<String>,
}

/// Every process (or those of one application) from the latest tick.
async fn processes(State(st): State<St>, Query(q): Query<ProcQ>) -> Response {
    let Some(all) = st.hub.detail("processes") else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no process data yet").into_response();
    };
    let list: Vec<&Value> = match all.as_array() {
        Some(a) => a
            .iter()
            .filter(|p| {
                q.app
                    .as_deref()
                    .is_none_or(|app| p["app"].as_str() == Some(app))
            })
            .collect(),
        None => Vec::new(),
    };
    Json(json!({"app": q.app, "now_ms": now_ms(), "processes": list})).into_response()
}

#[derive(Deserialize)]
struct PidQ {
    pid: u32,
}

fn json_error(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

/// Details of one process, read from /proc/<pid> on request, with its CPU
/// use, parent and children from the latest processes tick.
async fn process(State(st): State<St>, Query(q): Query<PidQ>) -> Response {
    if q.pid == 0 {
        return json_error(StatusCode::BAD_REQUEST, "invalid pid");
    }
    let Ok(permit) = st.proc_slots.clone().try_acquire_owned() else {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many process reads in progress",
        );
    };
    let roots = st.roots.clone();
    let pid = q.pid;
    let task = tokio::task::spawn_blocking(move || {
        // The permit is held until the read returns, even after a timeout.
        let _permit = permit;
        let users = crate::util::users::passwd_names();
        crate::procinfo::inspect(&roots, pid, &users)
    });
    let mut info = match tokio::time::timeout(PROC_TIMEOUT, task).await {
        Ok(Ok(Some(v))) => v,
        Ok(Ok(None)) => {
            return json_error(
                StatusCode::NOT_FOUND,
                "process not found (it may have exited)",
            );
        }
        Ok(Err(_)) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "process read failed"),
        Err(_) => {
            return json_error(
                StatusCode::GATEWAY_TIMEOUT,
                "reading /proc timed out (the process may be stuck in the kernel)",
            );
        }
    };
    let all = st.hub.detail("processes");
    let rows: &[Value] = all
        .as_deref()
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let ppid = info["ppid"].as_u64().unwrap_or(0);
    let pick = |p: &Value| {
        json!({
            "pid": p["pid"], "app": p["app"], "kind": p["kind"], "user": p["user"],
            "cpu": p["cpu"], "rss": p["rss"], "threads": p["threads"], "state": p["state"],
        })
    };
    let mut children: Vec<Value> = rows
        .iter()
        .filter(|p| p["ppid"].as_u64() == Some(pid as u64))
        .map(pick)
        .collect();
    children.sort_by(|a, b| {
        let f = |v: &Value| v["cpu"].as_f64().unwrap_or(0.0);
        f(b).total_cmp(&f(a))
    });
    children.truncate(200);
    if let Some(o) = info.as_object_mut() {
        if let Some(me) = rows.iter().find(|p| p["pid"].as_u64() == Some(pid as u64)) {
            o.insert("cpu".into(), me["cpu"].clone());
        }
        if let Some(parent) = rows.iter().find(|p| p["pid"].as_u64() == Some(ppid)) {
            o.insert("parent".into(), pick(parent));
        }
        o.insert("children".into(), Value::Array(children));
        o.insert("now_ms".into(), json!(now_ms()));
    }
    Json(info).into_response()
}

async fn get_settings(State(st): State<St>) -> Json<Value> {
    let s = st.settings_tx.borrow().clone();
    Json(json!(*s))
}

async fn put_settings(State(st): State<St>, body: axum::body::Bytes) -> Response {
    if body.len() > 64 * 1024 {
        return (StatusCode::PAYLOAD_TOO_LARGE, "settings too large").into_response();
    }
    let patch: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")).into_response(),
    };
    let cur = st.settings_tx.borrow().clone();
    match cur.apply_patch(&patch) {
        Ok(new) => {
            if let Some(s) = &st.storage {
                s.save_settings(&new);
            }
            let new = Arc::new(new);
            st.settings_tx.send_replace(new.clone());
            st.hub.send("settings", json!(*new));
            tracing::info!("settings updated");
            Json(json!(*new)).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, format!("invalid settings: {e}")).into_response(),
    }
}

async fn status(State(st): State<St>) -> Json<Value> {
    let backups = st.backups.list();
    Json(json!({
        "instance": st.hub.instance,
        "now_ms": now_ms(),
        "uptime_s": (now_ms() - st.hub.started_ms) / 1000,
        "overall": st.hub.overall_state(),
        "collectors": st.hub.statuses(),
        "storage": st.storage.as_ref().map(|s| s.status()),
        "memory_only": st.storage.is_none(),
        "storage_error": st.storage_error,
        "dropped_series": st.hub.dropped_series(),
        "sse_clients": st.sse_clients.load(Ordering::Relaxed),
        "backups": {
            "dir": st.backups.dir,
            "count": backups.len(),
            "latest": backups.first(),
            "total_bytes": backups.iter().map(|b| b.size).sum::<u64>(),
            "last_error": st.backups.last_error(),
        },
        "notes": st.notes,
    }))
}

async fn list_backups(State(st): State<St>) -> Json<Value> {
    let list = st.backups.list();
    Json(json!({
        "dir": st.backups.dir,
        "backups": list,
        "total_bytes": list.iter().map(|b| b.size).sum::<u64>(),
        "last_error": st.backups.last_error(),
        "available": st.storage.is_some(),
    }))
}

async fn create_backup(State(st): State<St>) -> Response {
    if st.storage.is_none() {
        return (StatusCode::CONFLICT, "no database (memory-only mode)").into_response();
    }
    if let Some(s) = &st.storage {
        s.flush(Duration::from_secs(5));
    }
    let settings = st.settings_tx.borrow().clone();
    match st.backups.run_now(&settings).await {
        Ok(info) => Json(json!(info)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("backup failed: {e:#}"),
        )
            .into_response(),
    }
}
