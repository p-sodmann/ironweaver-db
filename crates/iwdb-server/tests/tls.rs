//! TLS and mTLS (step 15b, ADR 0048): the handshake over gRPC and REST, a
//! wrong CA and expired certificates refused, client certificates mapped
//! to users and their roles, a required client certificate, the Secure
//! cookie, the probe, and reloading the certificate. The certificates are
//! the test-only fixtures of `tests/fixtures/tls`.
//!
//! Needs `rest` (it compares both APIs); a gRPC-only build runs the gRPC
//! conformance suite over TLS (`support::served`).

#![cfg(feature = "rest")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use iwdb::{Embedded, QueryConfig, Secret, Store};
use iwdb_engine::Mutation;
use iwdb_query::exec::block_on;
use iwdb_query::{Code, CommitOptions, Database, Error, QueryOptions, Role};
use iwdb_server::auth::AuthMode;
use iwdb_server::client::{ClientTls, Remote, RestRemote};
use iwdb_server::config::ClientAuth;
use iwdb_server::tls::{ServerTls, TlsFiles};
use support::{ADMIN, FAST, Running, auth_settings, client_tls, options, tls_fixture};

const NS: &str = "default";
const AUTH: AuthMode = AuthMode { enabled: true };

/// A store with the admin [`ADMIN`], `ann` (read on `default`) and `bob`
/// (write on `default`); `nobody` is no user.
fn store() -> (Embedded, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options()).unwrap();
    let users = store.users().with_params(FAST);
    users.create(ADMIN.0, &Secret::new(ADMIN.1), true).unwrap();
    for (user, role) in [("ann", Role::Read), ("bob", Role::Write)] {
        users.create(user, &Secret::new(format!("{}-password", user)), false).unwrap();
        users.grant(user, NS, role).unwrap();
    }
    (Embedded::new(store, QueryConfig::default()).unwrap().with_auth(auth_settings()), dir)
}

fn serve(client_auth: Option<ClientAuth>) -> (Running<Embedded>, tempfile::TempDir) {
    let (db, dir) = store();
    (Running::start_tls(db, AUTH, client_auth), dir)
}

fn grpc(server: &Running<Embedded>, tls: &ClientTls) -> Remote {
    Remote::connect_tls(&server.endpoint(), tls).unwrap()
}

fn rest(server: &Running<Embedded>, tls: &ClientTls) -> RestRemote {
    RestRemote::connect_tls(&server.endpoint(), tls).unwrap()
}

fn node(id: &str) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: vec!["P".into()],
        attr: [("n".to_owned(), ironweaver_core::Value::Int(1))].into(),
        meta: Default::default(),
        expected_version: None,
    }
}

fn read<D: Database>(db: &D) -> Result<(), Error> {
    block_on(db.get_nodes(NS, vec!["x".into()], QueryOptions::default())).map(|_| ())
}

fn write<D: Database>(db: &D, id: &str) -> Result<(), Error> {
    block_on(db.commit(NS, vec![node(id)], CommitOptions::default())).map(|_| ())
}

/// A raw HTTP/1.1 exchange over TLS: the answer's text.
fn https(addr: SocketAddr, tls: &ClientTls, request: &str) -> std::io::Result<String> {
    let config = tls.rustls_config(&[b"http/1.1"]).unwrap();
    let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
    let connection = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
    let mut stream = rustls::StreamOwned::new(connection, TcpStream::connect(addr)?);
    stream.write_all(request.as_bytes())?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer)?;
    Ok(answer)
}

/// The certificate the server presents, and the protocol ALPN agreed on.
fn handshake(addr: SocketAddr, alpn: &[&[u8]]) -> (Vec<u8>, Option<Vec<u8>>) {
    let config = client_tls(None).rustls_config(alpn).unwrap();
    let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
    let mut connection = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
    let mut tcp = TcpStream::connect(addr).unwrap();
    while connection.is_handshaking() {
        connection.complete_io(&mut tcp).unwrap();
    }
    let cert = connection.peer_certificates().unwrap()[0].to_vec();
    (cert, connection.alpn_protocol().map(<[u8]>::to_vec))
}

fn assert_refused(r: Result<(), Error>, code: Code, why: &str) {
    let e = r.expect_err("refused");
    assert_eq!(e.code(), code, "{}", e);
    assert!(e.message().contains(why), "{:?} in {}", why, e);
}

// ---- the handshake ----

#[test]
fn grpc_and_rest_speak_tls_on_one_port() {
    let (server, _dir) = serve(None);
    let tls = client_tls(None);
    for c in [Box::new(grpc(&server, &tls)) as Box<dyn Login>, Box::new(rest(&server, &tls))] {
        c.log_in(ADMIN.0, ADMIN.1);
        assert_eq!(c.who(), "admin");
    }
    let r = rest(&server, &tls);
    block_on(r.login("bob", Secret::new("bob-password"))).unwrap();
    write(&r, "over-rest").unwrap();
    // ALPN: h2 for gRPC (and REST over HTTP/2), http/1.1 for REST
    assert_eq!(handshake(server.addr, &[b"h2"]).1.as_deref(), Some(&b"h2"[..]));
    assert_eq!(handshake(server.addr, &[b"http/1.1"]).1.as_deref(), Some(&b"http/1.1"[..]));
    assert_eq!(handshake(server.addr, &[b"h2", b"http/1.1"]).1.as_deref(), Some(&b"h2"[..]));
    // Health over TLS, without credentials
    let answer = https(server.addr, &tls, "GET /v1/health/ready HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert!(answer.unwrap().starts_with("HTTP/1.1 200"));
}

/// The two clients, logging in.
trait Login {
    fn log_in(&self, user: &str, password: &str);
    fn who(&self) -> String;
}

impl Login for Remote {
    fn log_in(&self, user: &str, password: &str) {
        block_on(self.login(user, Secret::new(password))).unwrap();
    }
    fn who(&self) -> String {
        block_on(self.whoami()).unwrap().0.name
    }
}

impl Login for RestRemote {
    fn log_in(&self, user: &str, password: &str) {
        block_on(self.login(user, Secret::new(password))).unwrap();
    }
    fn who(&self) -> String {
        block_on(self.whoami()).unwrap().0.name
    }
}

#[test]
fn a_plaintext_client_gets_no_answer_from_a_tls_server() {
    let (server, _dir) = serve(None);
    let plain = Remote::connect(&format!("http://{}", server.addr)).unwrap();
    let e = read(&plain).unwrap_err();
    assert_eq!(e.code(), Code::Unavailable);
    let plain = RestRemote::connect(&format!("http://{}", server.addr)).unwrap();
    assert_eq!(read(&plain).unwrap_err().code(), Code::Unavailable);
    // And TLS settings with an http:// endpoint are a mistake
    let e = Remote::connect_tls(&format!("http://{}", server.addr), &client_tls(None)).unwrap_err();
    assert!(e.message().contains("isn't an https:// endpoint"), "{}", e);
    let e = RestRemote::connect_tls(&format!("http://{}", server.addr), &client_tls(None)).unwrap_err();
    assert!(e.message().contains("isn't an https:// endpoint"), "{}", e);
}

#[test]
fn a_server_of_another_ca_is_refused() {
    let (server, _dir) = serve(None);
    let other = ClientTls { ca: Some(tls_fixture("other-ca.pem")), ..ClientTls::default() };
    for e in [read(&grpc(&server, &other)).unwrap_err(), read(&rest(&server, &other)).unwrap_err()] {
        assert_eq!(e.code(), Code::Unavailable, "{}", e);
    }
    for e in [read(&rest(&server, &other)).unwrap_err(), read(&grpc(&server, &other)).unwrap_err()] {
        assert!(e.message().contains("invalid peer certificate: UnknownIssuer"), "{}", e);
    }
    // The server's view: the handshake failed, nothing was served
    assert!(https(server.addr, &other, "GET /v1/health/ready HTTP/1.1\r\n\r\n").is_err());
}

fn serve_with(cert: &str, client_auth: Option<ClientAuth>) -> (Running<Embedded>, tempfile::TempDir) {
    let (db, dir) = store();
    let files = TlsFiles {
        cert: tls_fixture(&format!("{}.pem", cert)),
        key: tls_fixture(&format!("{}.key", cert)),
        client_ca: client_auth.map(|_| tls_fixture("ca.pem")),
        client_auth: client_auth.unwrap_or(ClientAuth::Optional),
    };
    let tls = Arc::new(ServerTls::load(files).unwrap());
    (Running::start_built(db, |s| s.auth(AUTH).tls(Some(tls))), dir)
}

#[test]
fn an_expired_server_certificate_is_refused() {
    let (server, _dir) = serve_with("server-expired", None);
    let e = read(&rest(&server, &client_tls(None))).unwrap_err();
    assert_eq!(e.code(), Code::Unavailable, "{}", e);
    assert!(e.message().contains("certificate expired"), "{}", e);
    assert_eq!(read(&grpc(&server, &client_tls(None))).unwrap_err().code(), Code::Unavailable);
}

#[test]
fn client_certificates_that_are_expired_or_of_another_ca_are_refused() {
    let (server, _dir) = serve(Some(ClientAuth::Optional));
    for cert in ["expired", "other-ca"] {
        let tls = client_tls(Some(cert));
        // gRPC and REST: the handshake fails (TLS 1.3 reports it on the
        // first read), so nothing is served, not even login
        let e = read(&grpc(&server, &tls)).unwrap_err();
        assert_eq!(e.code(), Code::Unavailable, "{}", cert);
        assert_eq!(read(&rest(&server, &tls)).unwrap_err().code(), Code::Unavailable, "{}", cert);
        let r = rest(&server, &tls);
        assert_eq!(block_on(r.login(ADMIN.0, Secret::new(ADMIN.1))).unwrap_err().code(), Code::Unavailable);
        let answer = https(server.addr, &tls, "GET /v1/health/ready HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(answer.is_err() || answer.unwrap().is_empty(), "{}", cert);
    }
}

// ---- mTLS: a certificate's user ----

#[test]
fn a_client_certificate_is_its_users_principal_with_its_roles() {
    let (server, _dir) = serve(Some(ClientAuth::Optional));
    let ann = (grpc(&server, &client_tls(Some("ann"))), rest(&server, &client_tls(Some("ann"))));
    let bob = (grpc(&server, &client_tls(Some("bob"))), rest(&server, &client_tls(Some("bob"))));
    let admin = (grpc(&server, &client_tls(Some("admin"))), rest(&server, &client_tls(Some("admin"))));
    assert_eq!((ann.0.who(), ann.1.who()), ("ann".into(), "ann".into()));
    assert_eq!(bob.0.who(), "bob");
    // ann reads, but may not write; bob writes; neither creates namespaces
    read(&ann.0).unwrap();
    read(&ann.1).unwrap();
    let denied = "user 'ann' may not Commit: it needs the 'write' role on namespace 'default'";
    assert_refused(write(&ann.0, "a"), Code::PermissionDenied, denied);
    assert_refused(write(&ann.1, "a"), Code::PermissionDenied, denied);
    write(&bob.0, "b1").unwrap();
    write(&bob.1, "b2").unwrap();
    let denied = "user 'bob' may not CreateNamespace: it needs the server-wide 'admin' role";
    assert_refused(create(&bob.0, "nope"), Code::PermissionDenied, denied);
    assert_refused(create(&bob.1, "nope"), Code::PermissionDenied, denied);
    // The admin's certificate: everything
    create(&admin.0, "by_cert_grpc").unwrap();
    create(&admin.1, "by_cert_rest").unwrap();
    // A token wins over the certificate: ann's certificate, bob's login
    let r = rest(&server, &client_tls(Some("ann")));
    block_on(r.login("bob", Secret::new("bob-password"))).unwrap();
    assert_eq!(r.who(), "bob");
    write(&r, "b3").unwrap();
}

fn create<D: Database>(db: &D, name: &str) -> Result<(), Error> {
    block_on(db.create_namespace(name, None)).map(|_| ())
}

#[test]
fn certificates_that_name_no_user_are_refused() {
    let (server, _dir) = serve(Some(ClientAuth::Optional));
    for (cert, why) in [
        ("nobody", "the client certificate names no user of this server"),
        ("no-cn", "has no common name"),
        ("two-cn", "more than one common name"),
    ] {
        assert_refused(read(&grpc(&server, &client_tls(Some(cert)))), Code::Unauthenticated, why);
        assert_refused(read(&rest(&server, &client_tls(Some(cert)))), Code::Unauthenticated, why);
        // Logging in still works: the certificate is only one authenticator
        let r = rest(&server, &client_tls(Some(cert)));
        block_on(r.login("ann", Secret::new("ann-password"))).unwrap();
        read(&r).unwrap();
    }
}

/// A browser sends a client certificate on its own, as it sends a cookie:
/// a REST write authenticated by a certificate alone needs the CSRF header.
#[test]
fn a_rest_write_by_certificate_needs_the_csrf_header() {
    let (server, _dir) = serve(Some(ClientAuth::Optional));
    let body = r#"{"mutations":[{"upsertNode":{"id":"c","labels":["P"]}}]}"#;
    let post = |csrf: &str| {
        format!(
            "POST /v1/namespaces/default/commit HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            csrf,
            body.len(),
            body
        )
    };
    let tls = client_tls(Some("bob"));
    let answer = https(server.addr, &tls, &post("")).unwrap();
    assert!(answer.starts_with("HTTP/1.1 403"), "{}", answer);
    assert!(answer.contains("a request authenticated by a client certificate must carry the x-iwdb-csrf header"));
    let answer = https(server.addr, &tls, &post("x-iwdb-csrf: 1\r\n")).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{}", answer);
    // Reads need no header
    let get = "GET /v1/namespaces HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    assert!(https(server.addr, &tls, get).unwrap().starts_with("HTTP/1.1 200"));
}

#[test]
fn a_required_client_certificate_leaves_only_health_and_the_pages_open() {
    let (server, _dir) = serve(Some(ClientAuth::Required));
    let none = client_tls(None);
    let why = "this server requires a client certificate (mTLS)";
    assert_refused(read(&grpc(&server, &none)), Code::Unauthenticated, why);
    assert_refused(read(&rest(&server, &none)), Code::Unauthenticated, why);
    // Not even a login, or a token
    let r = rest(&server, &none);
    let e = block_on(r.login(ADMIN.0, Secret::new(ADMIN.1))).unwrap_err();
    assert_eq!(e.code(), Code::Unauthenticated, "{}", e);
    let g = grpc(&server, &none);
    assert_eq!(block_on(g.login(ADMIN.0, Secret::new(ADMIN.1))).unwrap_err().code(), Code::Unauthenticated);
    // Health without a certificate: probes and load balancers have none
    let answer = https(server.addr, &none, "GET /v1/health/ready HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert!(answer.unwrap().starts_with("HTTP/1.1 200"));
    iwdb_server::health::probe(&server.addr.to_string(), true, Duration::from_secs(5)).unwrap();
    // With one: as without the requirement
    read(&grpc(&server, &client_tls(Some("ann")))).unwrap();
    let bob = rest(&server, &client_tls(Some("bob")));
    write(&bob, "b").unwrap();
    let r = rest(&server, &client_tls(Some("ann")));
    block_on(r.login(ADMIN.0, Secret::new(ADMIN.1))).unwrap();
    assert_eq!(r.who(), "admin");
}

#[test]
fn an_optional_client_certificate_is_optional() {
    let (server, _dir) = serve(Some(ClientAuth::Optional));
    let none = rest(&server, &client_tls(None));
    assert_refused(read(&none), Code::Unauthenticated, "this server needs credentials");
    none.log_in("ann", "ann-password");
    read(&none).unwrap();
}

/// Without a client CA the server asks for no certificate; one that a
/// client has is not sent, and authenticates nobody.
#[test]
fn without_a_client_ca_certificates_authenticate_nobody() {
    let (server, _dir) = serve(None);
    assert_refused(read(&rest(&server, &client_tls(Some("admin")))), Code::Unauthenticated, "needs credentials");
    assert_refused(read(&grpc(&server, &client_tls(Some("admin")))), Code::Unauthenticated, "needs credentials");
}

// ---- the cookie, the probe, reloading ----

#[test]
fn the_session_cookie_is_secure_over_tls() {
    let (server, _dir) = serve(None);
    let body = r#"{"user":"admin","password":"admin-password","cookie":true}"#;
    let request = format!(
        "POST /v1/auth/login HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let answer = https(server.addr, &client_tls(None), &request).unwrap();
    let cookie = answer.lines().find(|l| l.to_ascii_lowercase().starts_with("set-cookie:")).unwrap();
    for part in ["iwdb_session=iwdb_", "HttpOnly", "SameSite=Strict", "Secure"] {
        assert!(cookie.contains(part), "{} in {}", part, cookie);
    }
    let logout = "POST /v1/auth/logout HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    let token = cookie.split("iwdb_session=").nth(1).unwrap().split(';').next().unwrap();
    let logout = logout.replace("Host: x\r\n", &format!("Host: x\r\nauthorization: Bearer {}\r\n", token));
    let answer = https(server.addr, &client_tls(None), &logout).unwrap();
    let cleared = answer.lines().find(|l| l.to_ascii_lowercase().starts_with("set-cookie:")).unwrap();
    assert!(cleared.contains("Max-Age=0") && cleared.contains("Secure"), "{}", cleared);
}

#[test]
fn the_probe_speaks_tls() {
    let (server, _dir) = serve(None);
    let address = server.addr.to_string();
    iwdb_server::health::probe(&address, true, Duration::from_secs(5)).unwrap();
    assert!(iwdb_server::health::probe(&address, false, Duration::from_secs(2)).is_err());
    // An expired certificate is no reason for a probe to fail: it doesn't
    // verify, and sends nothing secret
    let (expired, _dir) = serve_with("server-expired", None);
    iwdb_server::health::probe(&expired.addr.to_string(), true, Duration::from_secs(5)).unwrap();
}

/// A reload serves the new certificate on new connections; a reload that
/// fails keeps the one in use.
#[test]
fn a_reload_swaps_the_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::copy(tls_fixture("server.pem"), &cert).unwrap();
    std::fs::copy(tls_fixture("server.key"), &key).unwrap();
    let files = TlsFiles { cert: cert.clone(), key: key.clone(), client_ca: None, client_auth: ClientAuth::Optional };
    let tls = Arc::new(ServerTls::load(files).unwrap());
    let (db, _store) = store();
    let server = Running::start_built(db, |s| s.auth(AUTH).tls(Some(tls.clone())));
    let first = std::fs::read(tls_fixture("server.pem")).unwrap();
    let renewed = std::fs::read(tls_fixture("server-renewed.pem")).unwrap();
    let der = |pem: &[u8]| {
        use rustls_pki_types::pem::PemObject;
        rustls_pki_types::CertificateDer::from_pem_slice(pem).unwrap().to_vec()
    };
    assert_eq!(handshake(server.addr, &[b"h2"]).0, der(&first));
    let client = server.client();
    client.log_in(ADMIN.0, ADMIN.1);
    std::fs::copy(tls_fixture("server-renewed.pem"), &cert).unwrap();
    std::fs::copy(tls_fixture("server-renewed.key"), &key).unwrap();
    tls.reload().unwrap();
    assert_eq!(handshake(server.addr, &[b"h2"]).0, der(&renewed));
    // The open connection goes on
    assert_eq!(client.who(), "admin");
    // A half-written renewal: the key of another certificate
    std::fs::copy(tls_fixture("server.key"), &key).unwrap();
    assert!(tls.reload().is_err());
    std::fs::write(&cert, "not a certificate").unwrap();
    let e = tls.reload().unwrap_err().to_string();
    assert!(e.contains("holds no PEM certificate"), "{}", e);
    assert_eq!(handshake(server.addr, &[b"h2"]).0, der(&renewed));
    let again = server.client();
    again.log_in(ADMIN.0, ADMIN.1);
    assert_eq!(again.who(), "admin");
}
