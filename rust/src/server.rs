use crate::{
    cloud::{Cloud, Update as CloudUpdate},
    model::{DatadVersion, Snapshot, UbusCall},
    neighbor_manager::Manager as NeighborManager,
    state,
};
use anyhow::Result;
use axum::{
    Json, Router,
    extract::Request,
    extract::{Query, State},
    http::{Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{convert::Infallible, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock, watch},
};
use tokio_stream::{StreamExt, wrappers::WatchStream};
use tower_http::limit::RequestBodyLimitLayer;

#[derive(Clone)]
pub struct App {
    inner: Arc<Inner>,
}
struct Inner {
    snapshot: RwLock<Snapshot>,
    tx: watch::Sender<Snapshot>,
    interval: Duration,
    _data_dir: PathBuf,
    token: Option<String>,
    cloud: RwLock<Cloud>,
    neighbor: Mutex<NeighborManager>,
}

impl App {
    pub async fn new(
        data_dir: PathBuf,
        interval: Duration,
        token: Option<String>,
        neighbor_enabled: bool,
    ) -> Result<Self> {
        let mut initial = state::collect(interval.as_millis() as u64).await;
        let mut neighbor = NeighborManager::new(neighbor_enabled);
        neighbor
            .tick(initial.fields.get("net").unwrap_or(&Value::Null))
            .await;
        initial.fields.insert("neighbor".into(), neighbor.status());
        let (tx, _) = watch::channel(initial.clone());
        let app = Self {
            inner: Arc::new(Inner {
                snapshot: RwLock::new(initial),
                tx,
                interval,
                cloud: RwLock::new(Cloud::load(&data_dir)),
                neighbor: Mutex::new(neighbor),
                _data_dir: data_dir,
                token,
            }),
        };
        app.spawn_sampler();
        Ok(app)
    }
    pub async fn snapshot(&self) -> Snapshot {
        self.inner.snapshot.read().await.clone()
    }
    fn spawn_sampler(&self) {
        let app = self.clone();
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(app.inner.interval);
            loop {
                timer.tick().await;
                let mut next = state::collect(app.inner.interval.as_millis() as u64).await;
                let mut neighbor = app.inner.neighbor.lock().await;
                neighbor
                    .tick(next.fields.get("net").unwrap_or(&Value::Null))
                    .await;
                next.fields.insert("neighbor".into(), neighbor.status());
                drop(neighbor);
                let mut old = app.inner.snapshot.write().await;
                let mut comparable = next.clone();
                comparable.ts = old.ts;
                if comparable != *old {
                    *old = next.clone();
                    let _ = app.inner.tx.send(next);
                } else {
                    old.ts = next.ts;
                }
            }
        });
    }
    pub async fn serve(self, addr: SocketAddr, require_auth: bool) -> Result<()> {
        let mut router = Router::new()
            .route("/", get(index))
            .route("/healthz", get(health))
            .route("/version", get(version))
            .route("/state", get(snapshot))
            .route("/events", get(events))
            .route("/capabilities", get(capabilities))
            .route("/ubus", get(ubus_list))
            .route("/ubus/list", get(ubus_list))
            .route("/ubus/call", post(ubus_call))
            .route("/control", post(control));
        if !require_auth {
            router = router
                .route(
                    "/cloud/config",
                    get(cloud_config_get).post(cloud_config_post),
                )
                .route("/cloud/status", get(cloud_status));
        }
        if require_auth {
            router = router.layer(middleware::from_fn_with_state(self.clone(), authenticate));
        }
        let router = router
            .layer(RequestBodyLimitLayer::new(1024 * 1024))
            .with_state(self.clone());
        let listener = TcpListener::bind(addr).await?;
        let result = axum::serve(listener, router)
            .with_graceful_shutdown(shutdown())
            .await;
        self.inner.neighbor.lock().await.shutdown().await;
        result?;
        Ok(())
    }
}

async fn authenticate(State(app): State<App>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if matches!(path, "/" | "/healthz" | "/auth/login" | "/auth/exchange")
        || app.inner.token.is_none()
    {
        return next.run(request).await;
    }
    let wanted = app.inner.token.as_deref().unwrap_or_default();
    let headers = request.headers();
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let legacy = headers.get("x-auth-token").and_then(|v| v.to_str().ok());
    let query = request.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|part| part.strip_prefix("access_token="))
    });
    if [bearer, legacy, query]
        .into_iter()
        .flatten()
        .any(|value| constant_time_eq(value.as_bytes(), wanted.as_bytes()))
    {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, Json(json!({"ok":false,"error":{"code":"unauthorized","message":"authentication required"}}))).into_response()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

async fn index() -> &'static str {
    "zwrt-datad Rust rewrite\n"
}
async fn health() -> &'static str {
    "ok\n"
}
async fn version() -> Json<DatadVersion> {
    Json(Default::default())
}
async fn snapshot(State(app): State<App>) -> Json<Snapshot> {
    Json(app.snapshot().await)
}
async fn capabilities() -> Json<Value> {
    Json(json!({"protocol":1,"events":["state"],"rewrite":"rust"}))
}

async fn events(State(app): State<App>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = WatchStream::new(app.inner.tx.subscribe())
        .map(|v| Ok(Event::default().event("state").json_data(v).unwrap()));
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::new())
}
#[derive(Deserialize)]
struct ListQuery {
    verbose: Option<u8>,
}
async fn ubus_list(Query(q): Query<ListQuery>) -> Response {
    result(state::ubus_list(q.verbose == Some(1)).await)
}
async fn ubus_call(Json(req): Json<UbusCall>) -> Response {
    let service = req.service.clone();
    let method = req.method.clone();
    match state::ubus(&req.service, &req.method, req.args).await {
        Ok(value) => (
            StatusCode::OK,
            Json(json!({"ok":true,"service":service,"method":method,"result":value})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"ok":false,"error":{"code":"device_call_failed","message":e}})),
        )
            .into_response(),
    }
}
async fn control(State(app): State<App>, method: Method, Json(body): Json<Value>) -> Response {
    let _ = method;
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if action == "neighbor.status" {
        return (StatusCode::OK,Json(json!({"ok":true,"action":action,"result":app.inner.neighbor.lock().await.status()}))).into_response();
    }
    if action == "neighbor.set" {
        let enabled = body
            .get("params")
            .and_then(|v| v.get("enabled"))
            .and_then(Value::as_bool);
        let Some(enabled) = enabled else {
            return (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"action":action,"error":{"code":"invalid_parameter","message":"enabled must be boolean"}}))).into_response();
        };
        return match app.inner.neighbor.lock().await.set_enabled(enabled).await {Ok(value)=>(StatusCode::OK,Json(json!({"ok":true,"action":action,"result":value}))).into_response(),Err(error)=>(StatusCode::BAD_GATEWAY,Json(json!({"ok":false,"action":action,"error":{"code":"device_call_failed","message":error}}))).into_response()};
    }
    (StatusCode::NOT_IMPLEMENTED, Json(json!({"ok":false,"action":body.get("action"),"error":{"code":"rust_port_in_progress","message":"control adapter has not been migrated"}}))).into_response()
}
async fn cloud_config_get(State(app): State<App>) -> Json<Value> {
    Json(app.inner.cloud.read().await.public_config())
}
async fn cloud_status(State(app): State<App>) -> Json<Value> {
    Json(app.inner.cloud.read().await.status())
}
async fn cloud_config_post(State(app): State<App>, Json(update): Json<CloudUpdate>) -> Response {
    match app.inner.cloud.write().await.update(update) {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response(),
    }
}
fn result(value: Result<Value, String>) -> Response {
    match value {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"ok":false,"error":e}))).into_response(),
    }
}
async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_shape() {
        let v = serde_json::to_value(DatadVersion::default()).unwrap();
        assert_eq!(v["name"], "zwrt-datad");
    }
}
