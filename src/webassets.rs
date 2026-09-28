use axum::{
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
};

#[cfg(feature = "embedded-ui")]
include!(concat!(env!("OUT_DIR"), "/assets.rs"));

pub async fn serve(uri: Uri) -> Response {
    if uri.path().starts_with("/api/") || uri.path().starts_with("/health/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    #[cfg(feature = "embedded-ui")]
    {
        let path = if uri.path() == "/" {
            "/index.html"
        } else {
            uri.path()
        };
        if let Some((_, bytes)) = ASSETS.iter().find(|(name, _)| *name == path) {
            let mime = if path.ends_with(".html") {
                "text/html; charset=utf-8"
            } else if path.ends_with(".js") {
                "text/javascript; charset=utf-8"
            } else if path.ends_with(".css") {
                "text/css; charset=utf-8"
            } else {
                "application/octet-stream"
            };
            return (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                *bytes,
            )
                .into_response();
        }
    }
    #[cfg(not(feature = "embedded-ui"))]
    if uri.path() == "/" {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "UI assets are not embedded. Build web and enable embedded-ui.",
        )
            .into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}
