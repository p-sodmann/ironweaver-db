//! Who is calling (step 15a, ADRs 0045 and 0046): the gate authenticates
//! each request once and attaches a [`Caller`]; the adapters turn it into
//! an [`Authorized`] database, which decides. No adapter checks a role.
//!
//! **Credentials.** `authorization: Bearer <token>` (gRPC metadata or HTTP
//! header), or over REST the console's session cookie [`SESSION_COOKIE`]
//! (HttpOnly, SameSite=Strict). A request authenticated by the cookie with
//! a method other than GET or HEAD must also carry [`CSRF_HEADER`]: a page
//! of another origin can't send a custom header without a CORS preflight,
//! which the server doesn't answer (ADR 0046).
//!
//! **Open by design**: health (gRPC `grpc.health.v1`, `/v1/health/*`),
//! the console's pages, `Login` (`POST /v1/auth/login`) and the OpenAPI
//! document. Every other call needs a principal.

use std::net::IpAddr;
use std::sync::Arc;

use iwdb_query::{Accounts, Authenticate, Authorized, Code, Database, Error, Principal, Secret};

/// What the server serves: a database, its users, and logging in.
/// `iwdb::Embedded` is one.
pub trait Served: Database + Accounts + Authenticate + 'static {}

impl<D: Database + Accounts + Authenticate + 'static> Served for D {}

/// The console's session cookie.
pub const SESSION_COOKIE: &str = "iwdb_session";
/// The header a cookie-authenticated request other than GET or HEAD must
/// carry (any value).
pub const CSRF_HEADER: &str = "x-iwdb-csrf";
/// The REST login route.
pub const LOGIN_PATH: &str = "/v1/auth/login";
/// The gRPC login method.
pub const GRPC_LOGIN: &str = "/ironweaver_db.v1.AuthService/Login";
/// The gRPC service of `auth.proto`.
pub const GRPC_AUTH_PREFIX: &str = "/ironweaver_db.v1.AuthService/";

/// The caller of a request, as the gate found it: its principal (`None`
/// for the open routes), the token it sent, and its address.
#[derive(Clone, Debug)]
pub struct Caller {
    pub principal: Option<Arc<Principal>>,
    pub token: Option<Secret>,
    pub client: Option<IpAddr>,
}

impl Caller {
    /// A caller of a server without authentication: a server-wide admin.
    pub fn unauthenticated(client: Option<IpAddr>) -> Self {
        Caller { principal: Some(Arc::new(Principal::unauthenticated())), token: None, client }
    }
}

/// How the server authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct AuthMode {
    /// Check credentials (`[auth] enabled`). Off, every caller is a
    /// server-wide admin.
    pub enabled: bool,
}

fn unauthenticated(message: &str) -> Error {
    Error::new(Code::Unauthenticated, message)
}

/// A token from the request's headers, and whether it came from the
/// cookie.
pub(crate) fn token<B>(request: &http::Request<B>) -> Result<Option<(Secret, bool)>, Error> {
    let headers = request.headers();
    if let Some(value) = headers.get(http::header::AUTHORIZATION) {
        let text = value.to_str().map_err(|_| unauthenticated("the authorization header isn't text"))?;
        let (scheme, token) = text.split_once(' ').unwrap_or((text, ""));
        if !scheme.eq_ignore_ascii_case("bearer") || token.trim().is_empty() {
            return Err(unauthenticated("the authorization header must be 'Bearer <token>'"));
        }
        return Ok(Some((Secret::new(token.trim()), false)));
    }
    for value in headers.get_all(http::header::COOKIE) {
        let Ok(text) = value.to_str() else { continue };
        for pair in text.split(';') {
            if let Some((name, token)) = pair.trim().split_once('=')
                && name == SESSION_COOKIE
                && !token.is_empty()
            {
                return Ok(Some((Secret::new(token), true)));
            }
        }
    }
    Ok(None)
}

/// Whether a request needs no principal.
pub(crate) fn open<B>(request: &http::Request<B>, grpc: bool) -> bool {
    let path = request.uri().path();
    if grpc { path == GRPC_LOGIN } else { path == LOGIN_PATH || path == "/v1/openapi.json" }
}

/// What the gate found in a request's headers: a caller already, or a
/// token to look up.
pub(crate) enum Credentials {
    Caller(Caller),
    Token(Secret, Option<IpAddr>),
}

/// Read a request's credentials (once, in the gate). Errors:
/// `unauthenticated` (none, or malformed), `permission_denied` (a cookie
/// without the CSRF header on a request that writes).
pub(crate) fn credentials<B>(
    mode: AuthMode,
    request: &http::Request<B>,
    grpc: bool,
    client: Option<IpAddr>,
) -> Result<Credentials, Error> {
    if !mode.enabled {
        return Ok(Credentials::Caller(Caller::unauthenticated(client)));
    }
    let found = token(request)?;
    if open(request, grpc) {
        return Ok(Credentials::Caller(Caller { principal: None, token: found.map(|(t, _)| t), client }));
    }
    let Some((token, cookie)) = found else {
        return Err(unauthenticated(
            "this server needs credentials: log in (POST /v1/auth/login, or the Login RPC) and send \
             'authorization: Bearer <token>'",
        ));
    };
    let safe = matches!(*request.method(), http::Method::GET | http::Method::HEAD);
    if cookie && !safe && !request.headers().contains_key(CSRF_HEADER) {
        return Err(Error::new(
            Code::PermissionDenied,
            format!("a request authenticated by the session cookie must carry the {} header", CSRF_HEADER),
        ));
    }
    Ok(Credentials::Token(token, client))
}

/// The caller of `credentials`: a token's principal looked up. Errors:
/// `unauthenticated`.
pub(crate) async fn authenticate<D: Served>(db: &D, credentials: Credentials) -> Result<Caller, Error> {
    match credentials {
        Credentials::Caller(caller) => Ok(caller),
        Credentials::Token(token, client) => {
            let principal = db.authenticate(&token).await?;
            Ok(Caller { principal: Some(Arc::new(principal)), token: Some(token), client })
        }
    }
}

/// The database as the caller may use it. A request without a caller (a
/// server's service used without its gate) is a server-wide admin's if
/// authentication is off, and refused otherwise.
pub(crate) fn authorized<D>(db: &Arc<D>, mode: AuthMode, caller: Option<&Caller>) -> Result<Authorized<D>, Error> {
    let principal = match caller.and_then(|c| c.principal.clone()) {
        Some(principal) => principal,
        None if !mode.enabled => Arc::new(Principal::unauthenticated()),
        None => return Err(unauthenticated("this call needs credentials")),
    };
    Ok(Authorized::new(db.clone(), principal))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(headers: &[(&str, &str)]) -> http::Request<()> {
        let mut r = http::Request::builder().uri("/v1/namespaces");
        for (k, v) in headers {
            r = r.header(*k, *v);
        }
        r.body(()).expect("request")
    }

    #[test]
    fn tokens_come_from_the_bearer_header_or_the_cookie() {
        let t = |h: &[(&str, &str)]| token(&request(h)).map(|o| o.map(|(s, c)| (s.expose().to_owned(), c)));
        assert_eq!(t(&[]), Ok(None));
        assert_eq!(t(&[("authorization", "Bearer abc")]), Ok(Some(("abc".into(), false))));
        assert_eq!(t(&[("authorization", "bearer  abc ")]), Ok(Some(("abc".into(), false))));
        assert!(t(&[("authorization", "Basic abc")]).is_err());
        assert!(t(&[("authorization", "Bearer ")]).is_err());
        assert_eq!(t(&[("cookie", "a=b; iwdb_session=xyz; c=d")]), Ok(Some(("xyz".into(), true))));
        assert_eq!(t(&[("cookie", "iwdb_session_x=1")]), Ok(None));
        // The header wins over the cookie
        assert_eq!(
            t(&[("cookie", "iwdb_session=xyz"), ("authorization", "Bearer abc")]),
            Ok(Some(("abc".into(), false)))
        );
    }

    #[test]
    fn errors_never_echo_a_token() {
        let e = token(&request(&[("authorization", "Basic c2VjcmV0")])).expect_err("refused");
        assert!(!e.message().contains("c2VjcmV0"), "{}", e);
    }
}
