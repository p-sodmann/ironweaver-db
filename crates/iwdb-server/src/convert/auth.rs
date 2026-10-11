//! Users, roles and tokens (`auth.proto`, step 15a), both ways.

use std::collections::BTreeMap;

use iwdb_query::{
    ClientLimits, Error, LimitsReport, NamespaceLimits, NamespaceLimitsInfo, NewToken, Role, Secret, TokenInfo,
    UserInfo,
};

use crate::proto as pb;

pub(crate) fn role_to_pb(role: Role) -> pb::Role {
    match role {
        Role::Read => pb::Role::Read,
        Role::Write => pb::Role::Write,
        Role::Admin => pb::Role::Admin,
    }
}

pub(crate) fn role_from_pb(role: i32) -> Result<Role, Error> {
    match pb::Role::try_from(role) {
        Ok(pb::Role::Read) => Ok(Role::Read),
        Ok(pb::Role::Write) => Ok(Role::Write),
        Ok(pb::Role::Admin) => Ok(Role::Admin),
        _ => Err(Error::invalid("a role must be read, write or admin")),
    }
}

pub(crate) fn user_to_pb(user: &UserInfo) -> pb::User {
    pb::User {
        name: user.name.clone(),
        admin: user.admin,
        grants: user.grants.iter().map(|(ns, r)| (ns.clone(), role_to_pb(*r) as i32)).collect(),
        limits: Some(client_limits_to_pb(&user.limits)),
    }
}

pub(crate) fn user_from_pb(user: Option<pb::User>) -> Result<UserInfo, Error> {
    let user = user.ok_or_else(|| Error::invalid("the answer has no user"))?;
    let grants =
        user.grants.into_iter().map(|(ns, r)| Ok((ns, role_from_pb(r)?))).collect::<Result<BTreeMap<_, _>, Error>>()?;
    Ok(UserInfo { name: user.name, admin: user.admin, grants, limits: client_limits_from_pb(user.limits) })
}

pub(crate) fn client_limits_to_pb(l: &ClientLimits) -> pb::ClientLimits {
    pb::ClientLimits {
        requests_per_second: l.requests_per_second,
        burst: l.burst,
        max_running: l.max_running,
        max_results: l.max_results,
        max_visited: l.max_visited,
        timeout_ms: l.timeout_ms,
    }
}

/// Limits from the wire; a missing message is no limits set.
pub(crate) fn client_limits_from_pb(l: Option<pb::ClientLimits>) -> ClientLimits {
    let l = l.unwrap_or_default();
    ClientLimits {
        requests_per_second: l.requests_per_second,
        burst: l.burst,
        max_running: l.max_running,
        max_results: l.max_results,
        max_visited: l.max_visited,
        timeout_ms: l.timeout_ms,
    }
}

pub(crate) fn namespace_limits_to_pb(l: &NamespaceLimits) -> pb::NamespaceLimits {
    pb::NamespaceLimits { max_running: l.max_running, max_memory_bytes: l.max_memory_bytes }
}

pub(crate) fn namespace_limits_from_pb(l: Option<pb::NamespaceLimits>) -> NamespaceLimits {
    let l = l.unwrap_or_default();
    NamespaceLimits { max_running: l.max_running, max_memory_bytes: l.max_memory_bytes }
}

pub(crate) fn namespace_limits_info_to_pb(n: &NamespaceLimitsInfo) -> pb::SetNamespaceLimitsResponse {
    pb::SetNamespaceLimitsResponse { namespace: n.namespace.clone(), limits: Some(namespace_limits_to_pb(&n.limits)) }
}

pub(crate) fn namespace_limits_info_from_pb(r: pb::SetNamespaceLimitsResponse) -> NamespaceLimitsInfo {
    NamespaceLimitsInfo { namespace: r.namespace, limits: namespace_limits_from_pb(r.limits) }
}

pub(crate) fn limits_report_to_pb(r: &LimitsReport) -> pb::GetLimitsResponse {
    pb::GetLimitsResponse {
        client_defaults: Some(client_limits_to_pb(&r.client_defaults)),
        namespace_defaults: Some(namespace_limits_to_pb(&r.namespace_defaults)),
        namespaces: r
            .namespaces
            .iter()
            .map(|n| pb::NamespaceLimitsEntry {
                namespace: n.namespace.clone(),
                limits: Some(namespace_limits_to_pb(&n.limits)),
            })
            .collect(),
    }
}

pub(crate) fn limits_report_from_pb(r: pb::GetLimitsResponse) -> LimitsReport {
    LimitsReport {
        client_defaults: client_limits_from_pb(r.client_defaults),
        namespace_defaults: namespace_limits_from_pb(r.namespace_defaults),
        namespaces: r
            .namespaces
            .into_iter()
            .map(|n| NamespaceLimitsInfo { namespace: n.namespace, limits: namespace_limits_from_pb(n.limits) })
            .collect(),
    }
}

pub(crate) fn token_info_to_pb(info: &TokenInfo) -> pb::TokenInfo {
    pb::TokenInfo {
        user: info.user.clone(),
        name: info.name.clone(),
        created_ms: info.created_ms,
        expires_ms: info.expires_ms,
    }
}

pub(crate) fn token_info_from_pb(info: pb::TokenInfo) -> TokenInfo {
    TokenInfo { user: info.user, name: info.name, created_ms: info.created_ms, expires_ms: info.expires_ms }
}

pub(crate) fn new_token_to_pb(token: &NewToken) -> pb::CreateTokenResponse {
    pb::CreateTokenResponse { info: Some(token_info_to_pb(&token.info)), token: token.token.expose().to_owned() }
}

pub(crate) fn new_token_from_pb(response: pb::CreateTokenResponse) -> Result<NewToken, Error> {
    let info = response.info.ok_or_else(|| Error::invalid("the answer has no token info"))?;
    Ok(NewToken { info: token_info_from_pb(info), token: Secret::new(response.token) })
}
