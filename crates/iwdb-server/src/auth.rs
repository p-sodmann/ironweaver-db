//! Who is calling (step 15a, ADRs 0045 and 0046): the gate authenticates
//! each request once and attaches a [`Caller`]; the adapters turn it into
//! an [`Authorized`] database, which decides. No adapter checks a role.
//!
//! **Credentials.** `authorization: Bearer <token>` (gRPC metadata or HTTP
//! header), or over REST the console's session cookie [`SESSION_COOKIE`]
//! (HttpOnly, SameSite=Strict); without either, a verified client
//! certificate (mTLS, step 15b, ADR 0048) names the user. A REST request
//! authenticated by an ambient credential (the cookie, or a certificate: a
//! browser sends both on its own) with a method other than GET or HEAD
//! must also carry [`CSRF_HEADER`]: a page of another origin can't send a
//! custom header without a CORS preflight, which the server doesn't answer
//! (ADR 0046).
//!
//! **Client certificates required** (`[tls] client_auth = "required"`):
//! every request that reaches the gate's credentials needs one, whatever
//! else it carries; health and the console's pages are answered before.
//!
//! **Open by design**: health (gRPC `grpc.health.v1`, `/v1/health/*`),
//! the console's pages, `Login` (`POST /v1/auth/login`) and the OpenAPI
//! document. Every other call needs a principal.

use std::net::IpAddr;
use std::sync::Arc;

use iwdb_query::audit::{Audit, AuditEntry, AuditSink};
use iwdb_query::{Accounts, Authenticate, Authorized, Code, Database, Error, Operation, Principal, Secret, Via};

use crate::tls::ClientCertificate;

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
/// for the open routes), the token it sent, its address, and whether it
/// came over TLS.
#[derive(Clone, Debug)]
pub struct Caller {
    pub principal: Option<Arc<Principal>>,
    pub token: Option<Secret>,
    pub client: Option<IpAddr>,
    /// The request came over TLS (the session cookie is `Secure` then).
    pub tls: bool,
}

impl Caller {
    /// A caller of a server without authentication: a server-wide admin.
    pub fn unauthenticated(client: Option<IpAddr>) -> Self {
        Caller { principal: Some(Arc::new(Principal::unauthenticated())), token: None, client, tls: false }
    }
}

/// What the gate knows of a request's connection.
#[derive(Clone, Debug, Default)]
pub(crate) struct Connection {
    pub client: Option<IpAddr>,
    /// TLS, rather than plaintext.
    pub tls: bool,
    /// The client certificate the handshake verified.
    pub certificate: ClientCertificate,
    /// Every request past health and the console's pages needs a client
    /// certificate.
    pub certificate_required: bool,
}

impl Connection {
    /// A caller on this connection, without a principal yet.
    fn caller(&self, token: Option<Secret>) -> Caller {
        Caller { principal: None, token, client: self.client, tls: self.tls }
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

/// What the gate found in a request's headers and connection: a caller
/// already, a token to look up, or a certificate's user.
pub(crate) enum Credentials {
    Caller(Caller),
    Token(Secret, Caller),
    User(String, Caller),
}

/// A request authenticated by an ambient credential (one a browser sends
/// on its own) that may change something must carry the CSRF header. A
/// browser can't make a gRPC request of another origin (its content type
/// needs a preflight), so `grpc` requests are exempt.
fn check_csrf<B>(request: &http::Request<B>, grpc: bool, what: &str) -> Result<(), Error> {
    let safe = matches!(*request.method(), http::Method::GET | http::Method::HEAD);
    if !grpc && !safe && !request.headers().contains_key(CSRF_HEADER) {
        return Err(Error::new(
            Code::PermissionDenied,
            format!("a request authenticated by {} must carry the {} header", what, CSRF_HEADER),
        ));
    }
    Ok(())
}

/// The operation a request asks for, as far as the gate can tell (for the
/// audit entry of a refusal): a gRPC method of ours, or a REST route.
pub(crate) fn operation_of<B>(request: &http::Request<B>, grpc: bool) -> Option<Operation> {
    let path = request.uri().path();
    if grpc {
        let method = path.strip_prefix("/ironweaver_db.v1.")?.split_once('/')?.1;
        return Operation::from_rpc(method);
    }
    #[cfg(feature = "rest")]
    {
        crate::rest::operation_of(request.method(), path)
    }
    #[cfg(not(feature = "rest"))]
    None
}

/// Record a request the gate refused (ADR 0049): its operation if known,
/// the code, the client, and the certificate if that was the credential.
fn refused(audit: &dyn AuditSink, operation: Option<Operation>, connection: &Connection, via: Option<Via>, e: &Error) {
    audit.record(&AuditEntry {
        operation,
        code: Some(e.code()),
        via,
        client: connection.client,
        ..AuditEntry::default()
    });
}

/// Read a request's credentials (once, in the gate); a refusal is audited.
/// Errors: `unauthenticated` (none, or malformed; no client certificate
/// where one is required), `permission_denied` (an ambient credential
/// without the CSRF header on a request that writes).
pub(crate) fn credentials<B>(
    mode: AuthMode,
    request: &http::Request<B>,
    grpc: bool,
    connection: &Connection,
    audit: &dyn AuditSink,
) -> Result<Credentials, Error> {
    credentials_of(mode, request, grpc, connection).inspect_err(|e| {
        let via = match connection.certificate {
            ClientCertificate::None => None,
            _ => Some(Via::Certificate),
        };
        refused(audit, operation_of(request, grpc), connection, via, e)
    })
}

fn credentials_of<B>(
    mode: AuthMode,
    request: &http::Request<B>,
    grpc: bool,
    connection: &Connection,
) -> Result<Credentials, Error> {
    if connection.certificate_required && connection.certificate == ClientCertificate::None {
        return Err(unauthenticated("this server requires a client certificate (mTLS)"));
    }
    if !mode.enabled {
        let caller = Caller { tls: connection.tls, ..Caller::unauthenticated(connection.client) };
        return Ok(Credentials::Caller(caller));
    }
    let found = token(request)?;
    if open(request, grpc) {
        return Ok(Credentials::Caller(connection.caller(found.map(|(t, _)| t))));
    }
    match (found, &connection.certificate) {
        (Some((token, cookie)), _) => {
            if cookie {
                // Over gRPC too, as before mTLS: a cookie is the console's
                check_csrf(request, false, "the session cookie")?;
            }
            Ok(Credentials::Token(token, connection.caller(None)))
        }
        (None, ClientCertificate::User(user)) => {
            check_csrf(request, grpc, "a client certificate")?;
            Ok(Credentials::User(user.clone(), connection.caller(None)))
        }
        (None, ClientCertificate::Unnamed(why)) => Err(unauthenticated(why)),
        (None, ClientCertificate::None) => Err(unauthenticated(
            "this server needs credentials: log in (POST /v1/auth/login, or the Login RPC) and send \
             'authorization: Bearer <token>'",
        )),
    }
}

/// The caller of `credentials`: a token's or a certificate's principal
/// looked up; a refusal of `operation` is audited. Errors:
/// `unauthenticated`.
pub(crate) async fn authenticate<D: Served>(
    db: &D,
    credentials: Credentials,
    operation: Option<Operation>,
    connection: &Connection,
    audit: &dyn AuditSink,
) -> Result<Caller, Error> {
    match credentials {
        Credentials::Caller(caller) => Ok(caller),
        Credentials::Token(token, caller) => match db.authenticate(&token).await {
            Ok(principal) => Ok(Caller { principal: Some(Arc::new(principal)), token: Some(token), ..caller }),
            Err(e) => {
                refused(audit, operation, connection, None, &e);
                Err(e)
            }
        },
        Credentials::User(user, caller) => match db.principal_of(&user).await {
            Ok(principal) => Ok(Caller { principal: Some(Arc::new(principal)), ..caller }),
            Err(e) => {
                // Not the certificate's name: a certificate's contents
                // stay out of the audit log
                refused(audit, operation, connection, Some(Via::Certificate), &e);
                Err(e)
            }
        },
    }
}

/// The database as the caller may use it, auditing into `audit`. A
/// request without a caller (a server's service used without its gate) is
/// a server-wide admin's if authentication is off, and refused otherwise.
pub(crate) fn authorized<D>(
    db: &Arc<D>,
    mode: AuthMode,
    caller: Option<&Caller>,
    audit: &Arc<dyn AuditSink>,
) -> Result<Authorized<D>, Error> {
    let principal = match caller.and_then(|c| c.principal.clone()) {
        Some(principal) => principal,
        None if !mode.enabled => Arc::new(Principal::unauthenticated()),
        None => return Err(unauthenticated("this call needs credentials")),
    };
    Ok(Authorized::new(db.clone(), principal, Audit::new(audit.clone(), caller.and_then(|c| c.client))))
}

/// The audit context of a request without a principal (login).
pub(crate) fn audit_of(caller: Option<&Caller>, audit: &Arc<dyn AuditSink>) -> Audit {
    Audit::new(audit.clone(), caller.and_then(|c| c.client))
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
