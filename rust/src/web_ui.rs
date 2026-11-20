//! A compile-time allowlist of bundled assets, not a device filesystem server.
use crate::server::App;
use axum::{
    Router,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
mod bundled {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../web/dist/embedded.rs"
    ));
}
// Mantine and Motion set styles dynamically. Script execution stays same-origin;
// there are no inline scripts, eval, third-party resources or API cookies.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; worker-src 'self'; img-src 'self' blob: data:; font-src 'self'; base-uri 'self'; form-action 'none'; frame-ancestors 'none'; object-src 'none'";
fn asset(body: &'static [u8], content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_ENCODING, "gzip"),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_FRAME_OPTIONS, "DENY"),
        ],
        body,
    )
        .into_response()
}
pub fn routes() -> Router<App> {
    let mut router = Router::new().route(
        "/ui",
        get(|| async { (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, "ui/")]) }),
    );
    for &(path, body, mime) in bundled::ASSETS {
        router = router.route(path, get(move || async move { asset(body, mime) }));
    }
    router
}
