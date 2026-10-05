//! The operator console (step 16a, ADR 0037), served by the server itself
//! at `/console/` (feature `console`, turned on by `[console] enabled`;
//! ADR 0041).
//!
//! The pages are compiled into the binary from `console/`: the two pages,
//! their scripts and styles (the mock's too: the pages load it), the design
//! system's bundle and React. Nothing else under `console/` (tools, tests,
//! `serve.py`) is reachable, and no path is resolved on disk. The pages read and write through the REST API on the same origin
//! (`?source=rest`), so no CORS is needed and none is answered.
//!
//! The pages themselves are open (they hold no data); everything they read
//! and write goes through the REST API, behind the server's authentication
//! (step 15a): the pages show a login, and the session is an HttpOnly,
//! SameSite=Strict cookie (ADR 0046).

use axum::body::Body;
use http::{HeaderValue, Method, Response, StatusCode, header};

/// Where the console is served.
pub(crate) const PREFIX: &str = "/console";

macro_rules! files {
    ($($path:literal => $kind:literal),* $(,)?) => {
        /// Every file the console serves: its path under `/console/`, its
        /// media type and its bytes.
        pub(crate) const FILES: &[(&str, &str, &[u8])] = &[
            $(($path, $kind, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../console/", $path))),)*
        ];
    };
}

files! {
    "index.html" => "text/html; charset=utf-8",
    "status.html" => "text/html; charset=utf-8",
    "src/source.js" => "text/javascript; charset=utf-8",
    "src/mock.js" => "text/javascript; charset=utf-8",
    "src/rest.js" => "text/javascript; charset=utf-8",
    "src/query.js" => "text/javascript; charset=utf-8",
    "src/shared.js" => "text/javascript; charset=utf-8",
    "src/explorer.js" => "text/javascript; charset=utf-8",
    "src/status.js" => "text/javascript; charset=utf-8",
    "src/console.css" => "text/css; charset=utf-8",
    "design-system/tokens.css" => "text/css; charset=utf-8",
    "design-system/bundle.css" => "text/css; charset=utf-8",
    "design-system/bundle.js" => "text/javascript; charset=utf-8",
    "design-system/fonts/archivo-latin-wght-normal.woff2" => "font/woff2",
    "design-system/fonts/archivo-latin-ext-wght-normal.woff2" => "font/woff2",
    "design-system/fonts/ibm-plex-mono-latin-400-normal.woff2" => "font/woff2",
    "design-system/fonts/ibm-plex-mono-latin-500-normal.woff2" => "font/woff2",
    "design-system/fonts/ibm-plex-mono-latin-ext-400-normal.woff2" => "font/woff2",
    "design-system/fonts/ibm-plex-mono-latin-ext-500-normal.woff2" => "font/woff2",
    "design-system/fonts/OFL-Archivo.txt" => "text/plain; charset=utf-8",
    "design-system/fonts/OFL-IBM-Plex-Mono.txt" => "text/plain; charset=utf-8",
    "vendor/react.production.min.js" => "text/javascript; charset=utf-8",
    "vendor/react-dom.production.min.js" => "text/javascript; charset=utf-8",
    "vendor/LICENSE-react" => "text/plain; charset=utf-8",
}

/// Whether `path` is the console's.
pub(crate) fn serves(path: &str) -> bool {
    path == PREFIX || path.starts_with("/console/")
}

/// The answer to `method path` under [`PREFIX`]: a file, the console's
/// config, a redirect to the explorer on the server's REST API, 404 or 405.
pub(crate) fn answer(method: &Method, path: &str) -> Response<Body> {
    if method != Method::GET && method != Method::HEAD {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "the console only answers GET");
    }
    let rest = path.strip_prefix(PREFIX).unwrap_or_default();
    match rest {
        "" | "/" => redirect("/console/index.html?source=rest"),
        "/console-config.json" => {
            let config = format!(r#"{{"upstream":"this server","version":"{}"}}"#, env!("CARGO_PKG_VERSION"));
            file("application/json", config.into_bytes().into())
        }
        _ => match FILES.iter().find(|(p, ..)| rest.strip_prefix('/') == Some(*p)) {
            Some((_, kind, bytes)) => file(kind, Body::from(*bytes)),
            None => plain(StatusCode::NOT_FOUND, "no such page of the console"),
        },
    }
}

fn headers(mut response: Response<Body>, kind: &str) -> Response<Body> {
    let h = response.headers_mut();
    if let Ok(kind) = HeaderValue::from_str(kind) {
        h.insert(header::CONTENT_TYPE, kind);
    }
    // A new binary may bring new pages: browsers check before reusing
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    // The console writes to the database: no other site may frame it
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

fn file(kind: &str, body: Body) -> Response<Body> {
    headers(Response::new(body), kind)
}

fn plain(status: StatusCode, text: &'static str) -> Response<Body> {
    let mut response = headers(Response::new(Body::from(text)), "text/plain; charset=utf-8");
    *response.status_mut() = status;
    response
}

fn redirect(to: &'static str) -> Response<Body> {
    let mut response = headers(Response::new(Body::empty()), "text/plain; charset=utf-8");
    *response.status_mut() = StatusCode::FOUND;
    response.headers_mut().insert(header::LOCATION, HeaderValue::from_static(to));
    response
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const HTML: &str = "text/html; charset=utf-8";
    const JS: &str = "text/javascript; charset=utf-8";
    const CSS: &str = "text/css; charset=utf-8";

    /// Every file a page loads is served, and nothing else under console/
    /// but what the pages need.
    #[test]
    fn the_pages_find_every_file_they_load() {
        for page in ["index.html", "status.html"] {
            let (_, _, bytes) = FILES.iter().find(|(p, ..)| *p == page).unwrap();
            let text = std::str::from_utf8(bytes).unwrap();
            for attr in ["src=\"", "href=\""] {
                for part in text.split(attr).skip(1) {
                    let target = part.split('"').next().unwrap();
                    assert!(!target.contains("//"), "{} loads {} from elsewhere", page, target);
                    if target.starts_with('#') || target.is_empty() {
                        continue;
                    }
                    let target = target.split('?').next().unwrap();
                    assert!(FILES.iter().any(|(p, ..)| *p == target), "{} loads {}, which isn't served", page, target);
                }
            }
        }
        for (path, kind, _) in FILES {
            let expected = match path.rsplit('.').next() {
                Some("html") => HTML,
                Some("js") => JS,
                Some("css") => CSS,
                Some("woff2") => "font/woff2",
                _ => "text/plain; charset=utf-8",
            };
            assert_eq!(*kind, expected, "{}", path);
        }
    }

    /// The stylesheets load nothing from another site (step 15c: the
    /// fonts are served with the pages), and every `url()` is served.
    #[test]
    fn the_stylesheets_load_only_served_files() {
        for (path, ..) in FILES.iter().filter(|(_, kind, _)| *kind == CSS) {
            let (_, _, bytes) = FILES.iter().find(|(p, ..)| p == path).unwrap();
            let text = std::str::from_utf8(bytes).unwrap();
            assert!(!text.contains("@import"), "{} imports a stylesheet", path);
            let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
            for part in text.split("url(").skip(1) {
                let target = part.split(')').next().unwrap().trim_matches(|c| c == '"' || c == '\'');
                if target.starts_with("data:") {
                    continue;
                }
                assert!(!target.contains("//"), "{} loads {} from elsewhere", path, target);
                let full = format!("{}/{}", dir, target);
                assert!(FILES.iter().any(|(p, ..)| *p == full), "{} loads {}, which isn't served", path, full);
            }
        }
    }

    #[test]
    fn answers() {
        assert!(serves("/console") && serves("/console/x") && !serves("/consoles") && !serves("/v1/console"));
        let r = answer(&Method::GET, "/console");
        assert_eq!(r.status(), StatusCode::FOUND);
        assert_eq!(r.headers()[header::LOCATION], "/console/index.html?source=rest");
        let r = answer(&Method::GET, "/console/status.html");
        assert_eq!((r.status(), r.headers()[header::CONTENT_TYPE].to_str().unwrap()), (StatusCode::OK, HTML));
        assert_eq!(r.headers()[header::X_FRAME_OPTIONS], "DENY");
        for missing in
            ["/console/serve.py", "/console/../Cargo.toml", "/console/src/../serve.py", "/console/tools/seed.mjs"]
        {
            assert_eq!(answer(&Method::GET, missing).status(), StatusCode::NOT_FOUND, "{}", missing);
        }
        assert_eq!(answer(&Method::POST, "/console/index.html").status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(answer(&Method::GET, "/console/console-config.json").status(), StatusCode::OK);
    }
}
