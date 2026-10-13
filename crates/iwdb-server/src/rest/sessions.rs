//! Session and user-administration handlers.

use super::transport::{Answer, Failure, namespace, path, read, unary};
use super::{Caller_, St};
use crate::auth::{SESSION_COOKIE, Served, audit_of};
use crate::{ops, proto as pb};
use axum::Extension;
use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use http::{HeaderMap, HeaderValue, header};
use iwdb_query::Error;

// ---- authentication, users, grants, tokens (step 15a) ----

/// A field of a body that names what the path names already.
fn same(field: &mut String, path: &str, what: &str) -> Result<(), Failure> {
    if !field.is_empty() && field != path {
        return Err(Error::invalid(format!("the body names {} '{}', the path '{}'", what, field, path)).into());
    }
    *field = path.to_owned();
    Ok(())
}

/// The session cookie (ADR 0046): HttpOnly (no script reads it),
/// SameSite=Strict (no other site sends it), and `Secure` (sent over
/// HTTPS only) when the request came over TLS (step 15b): browsers drop
/// Secure cookies set over plain HTTP, which a server with
/// `[tls] enabled = false` speaks.
fn session_cookie(token: &str, max_age_secs: u64, secure: bool) -> HeaderValue {
    let text = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        SESSION_COOKIE,
        token,
        max_age_secs,
        if secure { "; Secure" } else { "" }
    );
    HeaderValue::from_str(&text).unwrap_or_else(|_| HeaderValue::from_static("iwdb_session=; Max-Age=0"))
}

/// `POST /v1/auth/login`: the session's token in the body, or with `cookie`
/// as the session cookie.
pub(super) async fn login<D: Served>(State(s): St<D>, caller: Caller_, headers: HeaderMap, body: Body) -> Answer {
    let r: pb::LoginRequest = read(&headers, body, s.max_body).await?;
    let cookie = r.cookie;
    let audit = audit_of(caller.as_ref().map(|Extension(c)| c), &s.audit);
    let secure = caller.as_ref().is_some_and(|Extension(c)| c.tls);
    let (response, session) = ops::login(&*s.db, r, &audit).await?;
    let mut answer = unary(Ok(response))?;
    if cookie {
        let max_age = session.expires_ms.saturating_sub(iwdb::auth::now_ms()) / 1000;
        answer.headers_mut().insert(header::SET_COOKIE, session_cookie(session.token.expose(), max_age, secure));
    }
    answer.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(answer)
}

/// `POST /v1/auth/logout`: ends the session and clears the cookie.
pub(super) async fn logout<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    let db = s.db(&caller)?;
    let token = caller.as_ref().and_then(|Extension(c)| c.token.clone());
    let secure = caller.as_ref().is_some_and(|Extension(c)| c.tls);
    let mut answer = unary(ops::logout(&db, token.as_ref()).await)?;
    answer.headers_mut().insert(header::SET_COOKIE, session_cookie("", 0, secure));
    Ok(answer)
}

pub(super) async fn who_am_i<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    unary(Ok(ops::who_am_i(&s.db(&caller)?, s.mode)))
}

pub(super) async fn list_users<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    unary(ops::list_users(&s.db(&caller)?).await)
}

pub(super) async fn create_user<D: Served>(State(s): St<D>, caller: Caller_, headers: HeaderMap, body: Body) -> Answer {
    let db = s.db(&caller)?;
    let r: pb::CreateUserRequest = read(&headers, body, s.max_body).await?;
    unary(ops::create_user(&db, r).await)
}

/// A handler of a route under `/v1/users/{user}` whose body is the RPC's
/// request message: the user (and namespace or token name) from the path.
macro_rules! user_handler {
    ($name:ident, $request:ty, $params:ty, |$r:ident, $p:ident| $fill:block, $op:ident) => {
        pub(super) async fn $name<D: Served>(
            State(s): St<D>,
            caller: Caller_,
            p: Result<Path<$params>, PathRejection>,
            headers: HeaderMap,
            body: Body,
        ) -> Answer {
            let db = s.db(&caller)?;
            let mut $r: $request = read(&headers, body, s.max_body).await?;
            let $p = path(p)?;
            $fill
            unary(ops::$op(&db, $r).await)
        }
    };
}

user_handler!(
    delete_user,
    pb::DeleteUserRequest,
    String,
    |r, user| {
        same(&mut r.name, &user, "user")?;
    },
    delete_user
);
user_handler!(
    set_password,
    pb::SetPasswordRequest,
    String,
    |r, user| {
        same(&mut r.name, &user, "user")?;
    },
    set_password
);
user_handler!(
    set_admin,
    pb::SetAdminRequest,
    String,
    |r, user| {
        same(&mut r.name, &user, "user")?;
    },
    set_admin
);
user_handler!(
    grant,
    pb::GrantRequest,
    (String, String),
    |r, p| {
        same(&mut r.name, &p.0, "user")?;
        namespace(&mut r.namespace, p.1)?;
    },
    grant
);
user_handler!(
    revoke,
    pb::RevokeRequest,
    (String, String),
    |r, p| {
        same(&mut r.name, &p.0, "user")?;
        namespace(&mut r.namespace, p.1)?;
    },
    revoke
);
user_handler!(
    create_token,
    pb::CreateTokenRequest,
    String,
    |r, user| {
        same(&mut r.user, &user, "user")?;
    },
    create_token
);
user_handler!(
    revoke_token,
    pb::RevokeTokenRequest,
    (String, String),
    |r, p| {
        same(&mut r.user, &p.0, "user")?;
        same(&mut r.name, &p.1, "token")?;
    },
    revoke_token
);

user_handler!(
    set_user_limits,
    pb::SetUserLimitsRequest,
    String,
    |r, user| {
        same(&mut r.name, &user, "user")?;
    },
    set_user_limits
);

pub(super) async fn get_limits<D: Served>(State(s): St<D>, caller: Caller_) -> Answer {
    unary(ops::get_limits(&s.db(&caller)?).await)
}

pub(super) async fn list_tokens<D: Served>(
    State(s): St<D>,
    caller: Caller_,
    p: Result<Path<String>, PathRejection>,
) -> Answer {
    let db = s.db(&caller)?;
    unary(ops::list_tokens(&db, pb::ListTokensRequest { user: path(p)? }).await)
}
