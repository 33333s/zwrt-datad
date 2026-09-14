use crate::{
    model::{DatadVersion, Snapshot, UbusCall},
    state,
};
use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{convert::Infallible, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{RwLock, watch},
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
}

impl App {
    pub async fn new(data_dir: PathBuf, interval: Duration) -> Result<Self> {
        let initial = state::collect().await;
        let (tx, _) = watch::channel(initial.clone());
        let app = Self {
            inner: Arc::new(Inner {
                snapshot: RwLock::new(initial),
                tx,
                interval,
                _data_dir: data_dir,
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
                let next = state::collect().await;
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
    pub async fn serve(self, addr: SocketAddr) -> Result<()> {
        let router = Router::new()
            .route("/", get(index))
            .route("/healthz", get(health))
            .route("/version", get(version))
            .route("/state", get(snapshot))
            .route("/events", get(events))
            .route("/capabilities", get(capabilities))
            .route("/ubus", get(ubus_list))
            .route("/ubus/list", get(ubus_list))
            .route("/ubus/call", post(ubus_call))
            .route("/control", post(control))
            .layer(RequestBodyLimitLayer::new(1024 * 1024))
            .with_state(self.clone());
        let listener = TcpListener::bind(addr).await?;
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown())
            .await?;
        Ok(())
    }
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
    let stream = WatchStream::new(app.inner.tx.subscribe()).map(|v| {
        Ok(Event::default()
            .event("state")
            .retry(Duration::from_secs(1))
            .json_data(v)
            .unwrap())
    });
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
async fn control(method: Method, Json(body): Json<Value>) -> Response {
    let _ = method;
    (StatusCode::NOT_IMPLEMENTED, Json(json!({"ok":false,"action":body.get("action"),"error":{"code":"rust_port_in_progress","message":"control adapter has not been migrated"}}))).into_response()
}
fn result(value: Result<Value, String>) -> Response {
    match value {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"ok":false,"error":e}))).into_response(),
    }
}
async fn shutdown() {
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
