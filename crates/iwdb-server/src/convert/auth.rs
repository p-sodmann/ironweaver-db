//! Users, roles and tokens (`auth.proto`, step 15a), both ways.

use std::collections::BTreeMap;

use iwdb_query::{Error, NewToken, Role, Secret, TokenInfo, UserInfo};

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
    }
}

pub(crate) fn user_from_pb(user: Option<pb::User>) -> Result<UserInfo, Error> {
    let user = user.ok_or_else(|| Error::invalid("the answer has no user"))?;
    let grants =
        user.grants.into_iter().map(|(ns, r)| Ok((ns, role_from_pb(r)?))).collect::<Result<BTreeMap<_, _>, Error>>()?;
    Ok(UserInfo { name: user.name, admin: user.admin, grants })
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
