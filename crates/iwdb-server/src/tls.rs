//! TLS and mTLS (step 15b, ADR 0048), with rustls and its ring provider.
//!
//! - [`ServerTls`]: the server's certificate and key, and with a client CA
//!   the verifier of client certificates; reloaded from its files by
//!   [`ServerTls::reload`] (the binary calls it on SIGHUP). One port serves
//!   gRPC and REST: ALPN offers `h2` and `http/1.1`.
//! - [`common_name`]: the user a verified client certificate names (its
//!   subject's common name).
//! - [`ClientTls`]: what a client trusts and presents (the clients and the
//!   probe).
//!
//! **Secrets.** A private key is read from its file, handed to rustls and
//! never stored elsewhere, logged or put in an error: errors name the
//! file and what is wrong with it, never its contents.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use rustls::RootCertStore;
use rustls::server::WebPkiClientVerifier;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use crate::config::ClientAuth;

/// TLS that can't be set up from its files.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("can't read the {what} {}: {source}", path.display())]
    Read { what: &'static str, path: PathBuf, source: std::io::Error },
    /// The file isn't PEM, or holds none of what it should.
    #[error("{} holds no {what}", path.display())]
    Pem { what: &'static str, path: PathBuf },
    /// rustls refused the material (a key that doesn't match its
    /// certificate, an unsupported key type, a CA it can't use).
    #[error("{0}")]
    Invalid(String),
}

/// The files of the server's TLS (`[tls]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsFiles {
    /// The certificate chain, the server's certificate first (PEM).
    pub cert: PathBuf,
    /// Its private key (PEM: PKCS#8, PKCS#1 or SEC1).
    pub key: PathBuf,
    /// The CAs client certificates are verified against (PEM): turns mTLS
    /// on.
    pub client_ca: Option<PathBuf>,
    /// Whether a client certificate is required (with `client_ca`).
    pub client_auth: ClientAuth,
}

fn read(what: &'static str, path: &Path) -> Result<Vec<u8>, TlsError> {
    std::fs::read(path).map_err(|source| TlsError::Read { what, path: path.to_owned(), source })
}

/// The certificates of a PEM file (at least one).
pub(crate) fn certificates(what: &'static str, path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let pem = read(what, path)?;
    let none = || TlsError::Pem { what: "PEM certificate", path: path.to_owned() };
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_slice_iter(&pem).collect::<Result<_, _>>().map_err(|_| none())?;
    if certs.is_empty() { Err(none()) } else { Ok(certs) }
}

/// The private key of a PEM file. Its errors never quote the file.
pub(crate) fn private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let pem = read("private key", path)?;
    PrivateKeyDer::from_pem_slice(&pem)
        .map_err(|_| TlsError::Pem { what: "PEM private key (PKCS#8, PKCS#1 or SEC1)", path: path.to_owned() })
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn invalid(what: &str, e: impl std::fmt::Display) -> TlsError {
    TlsError::Invalid(format!("{}: {}", what, e))
}

/// The CAs of a PEM file, as a root store.
fn roots(what: &'static str, path: &Path) -> Result<RootCertStore, TlsError> {
    let mut roots = RootCertStore::empty();
    for cert in certificates(what, path)? {
        roots.add(cert).map_err(|e| invalid(&format!("{} {}", what, path.display()), e))?;
    }
    Ok(roots)
}

/// The ALPN protocols of the one port: gRPC and REST over HTTP/2, REST
/// over HTTP/1.1.
const ALPN: [&[u8]; 2] = [b"h2", b"http/1.1"];

fn server_config(files: &TlsFiles) -> Result<rustls::ServerConfig, TlsError> {
    let certs = certificates("TLS certificate", &files.cert)?;
    let key = private_key(&files.key)?;
    let provider = provider();
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| invalid("TLS", e))?;
    let builder = match &files.client_ca {
        Some(ca) => {
            let roots = roots("client CA", ca)?;
            // A certificate that is presented is always verified; whether
            // one is required is the gate's to decide, so health stays
            // open (ADR 0048)
            let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                .allow_unauthenticated()
                .build()
                .map_err(|e| invalid(&format!("client CA {}", ca.display()), e))?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };
    let mut config = builder.with_single_cert(certs, key).map_err(|e| {
        invalid(&format!("TLS certificate {} and key {}", files.cert.display(), files.key.display()), e)
    })?;
    config.alpn_protocols = ALPN.iter().map(|p| p.to_vec()).collect();
    Ok(config)
}

/// The server's TLS: its files and the configuration they make. Reloads
/// swap the configuration at once; connections that are open keep theirs.
pub struct ServerTls {
    files: TlsFiles,
    current: RwLock<Arc<rustls::ServerConfig>>,
}

impl std::fmt::Debug for ServerTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerTls").field("files", &self.files).finish_non_exhaustive()
    }
}

impl ServerTls {
    /// Read the certificate, the key and the client CA. Errors: a file that
    /// can't be read or holds none of what it should, a key that doesn't
    /// match the certificate.
    pub fn load(files: TlsFiles) -> Result<ServerTls, TlsError> {
        let config = server_config(&files)?;
        Ok(ServerTls { files, current: RwLock::new(Arc::new(config)) })
    }

    /// Read the files again and use them for new connections. On an error
    /// the configuration in use stays.
    pub fn reload(&self) -> Result<(), TlsError> {
        let config = Arc::new(server_config(&self.files)?);
        match self.current.write() {
            Ok(mut current) => *current = config,
            Err(poisoned) => *poisoned.into_inner() = config,
        }
        Ok(())
    }

    pub fn files(&self) -> &TlsFiles {
        &self.files
    }

    /// Whether every request but health and the console's pages needs a
    /// client certificate.
    pub fn requires_client_certificate(&self) -> bool {
        self.files.client_ca.is_some() && self.files.client_auth == ClientAuth::Required
    }

    pub(crate) fn acceptor(&self) -> tokio_rustls::TlsAcceptor {
        let config = match self.current.read() {
            Ok(current) => current.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        tokio_rustls::TlsAcceptor::from(config)
    }
}

/// What a verified client certificate says about its user.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ClientCertificate {
    /// None was presented (or the connection is plaintext).
    #[default]
    None,
    /// A verified certificate whose subject's common name is this user.
    User(String),
    /// A verified certificate that names no user: why.
    Unnamed(&'static str),
}

impl ClientCertificate {
    /// From the end-entity certificate of a verified chain.
    pub fn of(der: Option<&[u8]>) -> ClientCertificate {
        match der.map(common_name) {
            None => ClientCertificate::None,
            Some(Ok(user)) => ClientCertificate::User(user),
            Some(Err(why)) => ClientCertificate::Unnamed(why),
        }
    }
}

// ---- the subject's common name, from the DER of an X.509 certificate ----

/// One DER element: its tag, its contents, and what follows it. Tags of
/// one byte only (X.509 uses no others where we read).
fn element(der: &[u8]) -> Result<(u8, &[u8], &[u8]), &'static str> {
    const BAD: &str = "the client certificate isn't valid DER";
    let (&tag, rest) = der.split_first().ok_or(BAD)?;
    if tag & 0x1f == 0x1f {
        return Err(BAD);
    }
    let (&first, rest) = rest.split_first().ok_or(BAD)?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 || rest.len() < n {
            return Err(BAD);
        }
        let len = rest[..n].iter().fold(0usize, |len, &b| (len << 8) | usize::from(b));
        (len, &rest[n..])
    };
    if rest.len() < len {
        return Err(BAD);
    }
    Ok((tag, &rest[..len], &rest[len..]))
}

/// The element of `tag` at the start of `der`, and what follows it.
fn expect(der: &[u8], tag: u8) -> Result<(&[u8], &[u8]), &'static str> {
    match element(der)? {
        (t, contents, rest) if t == tag => Ok((contents, rest)),
        _ => Err("the client certificate isn't an X.509 certificate"),
    }
}

const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;
const OID: u8 = 0x06;
/// 2.5.4.3, `commonName`.
const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];

/// The user a client certificate names: its subject's common name, which
/// must be there once, as UTF-8, PrintableString or IA5String text. The
/// certificate has been verified by then; this only reads it, and never
/// panics on any input.
pub fn common_name(der: &[u8]) -> Result<String, &'static str> {
    let (certificate, _) = expect(der, SEQUENCE)?;
    let (mut tbs, _) = expect(certificate, SEQUENCE)?;
    // version [0] EXPLICIT, optional
    if let Ok((0xa0, _, rest)) = element(tbs) {
        tbs = rest;
    }
    // serialNumber, signature, issuer, validity: skipped
    for _ in 0..4 {
        tbs = element(tbs)?.2;
    }
    let (mut subject, _) = expect(tbs, SEQUENCE)?;
    let mut names = Vec::new();
    while !subject.is_empty() {
        let (mut set, rest) = expect(subject, SET)?;
        subject = rest;
        while !set.is_empty() {
            let (attribute, rest) = expect(set, SEQUENCE)?;
            set = rest;
            let (oid, value) = expect(attribute, OID)?;
            if oid != COMMON_NAME {
                continue;
            }
            let (tag, text, _) = element(value)?;
            // UTF8String, PrintableString, IA5String
            if !matches!(tag, 0x0c | 0x13 | 0x16) {
                return Err("the client certificate's common name isn't text");
            }
            names.push(std::str::from_utf8(text).map_err(|_| "the client certificate's common name isn't UTF-8")?);
        }
    }
    match names.as_slice() {
        [name] => Ok((*name).to_owned()),
        [] => Err("the client certificate's subject has no common name (the user)"),
        _ => Err("the client certificate's subject has more than one common name"),
    }
}

// ---- clients ----

/// What a client trusts and presents over TLS: `ca` (PEM) to verify the
/// server against (default: the operating system's trust store), and a
/// client certificate and key (PEM) for mTLS.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientTls {
    pub ca: Option<PathBuf>,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

impl ClientTls {
    /// Whether anything is set: then the endpoint must be `https://`.
    pub fn is_set(&self) -> bool {
        self.ca.is_some() || self.cert.is_some() || self.key.is_some()
    }

    /// The client certificate and key, both or neither.
    pub fn identity(&self) -> Result<Option<(&Path, &Path)>, TlsError> {
        match (&self.cert, &self.key) {
            (Some(cert), Some(key)) => Ok(Some((cert, key))),
            (None, None) => Ok(None),
            _ => Err(TlsError::Invalid("a client certificate needs its key, and a key its certificate".into())),
        }
    }

    /// A rustls client configuration (REST's client), offering `alpn`.
    #[cfg(feature = "client")]
    pub fn rustls_config(&self, alpn: &[&[u8]]) -> Result<rustls::ClientConfig, TlsError> {
        let roots = match &self.ca {
            Some(ca) => roots("CA certificate", ca)?,
            None => {
                let mut roots = RootCertStore::empty();
                let found = rustls_native_certs::load_native_certs();
                roots.add_parsable_certificates(found.certs);
                if roots.is_empty() {
                    return Err(TlsError::Invalid(
                        "the operating system's trust store has no certificates: give the server's CA".into(),
                    ));
                }
                roots
            }
        };
        let builder = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| invalid("TLS", e))?
            .with_root_certificates(roots);
        let mut config = match self.identity()? {
            Some((cert, key)) => builder
                .with_client_auth_cert(certificates("client certificate", cert)?, private_key(key)?)
                .map_err(|e| invalid(&format!("client certificate {} and key {}", cert.display(), key.display()), e))?,
            None => builder.with_no_client_auth(),
        };
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        Ok(config)
    }
}

/// A client configuration that doesn't verify the server's certificate:
/// the probe's only (it sends no credentials and reads readiness, from
/// next to the server). The handshake's signatures are still checked.
pub(crate) fn unverified_client_config() -> Result<rustls::ClientConfig, TlsError> {
    let provider = provider();
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| invalid("TLS", e))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyServer(provider)))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

#[derive(Debug)]
struct AnyServer(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyServer {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/tls").join(name)
    }

    fn der(name: &str) -> Vec<u8> {
        certificates("test", &fixture(name)).unwrap().remove(0).to_vec()
    }

    #[test]
    fn the_common_name_is_the_user() {
        assert_eq!(common_name(&der("client-ann.pem")), Ok("ann".to_owned()));
        assert_eq!(common_name(&der("client-admin.pem")), Ok("admin".to_owned()));
        assert!(common_name(&der("client-no-cn.pem")).unwrap_err().contains("no common name"));
        assert!(common_name(&der("client-two-cn.pem")).unwrap_err().contains("more than one"));
    }

    /// Any bytes give an answer, never a panic: every prefix and every
    /// one-byte change of a real certificate.
    #[test]
    fn reading_the_common_name_never_panics() {
        let der = der("client-ann.pem");
        for n in 0..der.len() {
            let _ = common_name(&der[..n]);
        }
        for i in 0..der.len() {
            for b in [0x00, 0x7f, 0x80, 0x84, 0x85, 0xff] {
                let mut changed = der.clone();
                changed[i] = b;
                let _ = common_name(&changed);
            }
        }
        assert!(common_name(&[]).is_err());
        assert!(common_name(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff]).is_err());
    }

    fn files(cert: &str, key: &str) -> TlsFiles {
        TlsFiles { cert: fixture(cert), key: fixture(key), client_ca: None, client_auth: ClientAuth::Optional }
    }

    #[test]
    fn the_server_loads_its_files_and_says_what_is_wrong() {
        let tls = ServerTls::load(files("server.pem", "server.key")).unwrap();
        assert!(!tls.requires_client_certificate());
        tls.reload().unwrap();
        let e = ServerTls::load(files("missing.pem", "server.key")).unwrap_err().to_string();
        assert!(e.contains("can't read the TLS certificate") && e.contains("missing.pem"), "{}", e);
        let e = ServerTls::load(files("server.key", "server.key")).unwrap_err().to_string();
        assert!(e.contains("holds no PEM certificate"), "{}", e);
        let e = ServerTls::load(files("server.pem", "server.pem")).unwrap_err().to_string();
        assert!(e.contains("holds no PEM private key"), "{}", e);
        // Another certificate's key
        let e = ServerTls::load(files("server.pem", "client-ann.key")).unwrap_err().to_string();
        assert!(e.contains("server.pem") && e.contains("client-ann.key"), "{}", e);
        let mut mtls = files("server.pem", "server.key");
        mtls.client_ca = Some(fixture("ca.pem"));
        mtls.client_auth = ClientAuth::Required;
        assert!(ServerTls::load(mtls).unwrap().requires_client_certificate());
    }

    /// No error quotes a private key's file, whatever is wrong with it.
    #[test]
    fn errors_never_quote_a_private_key() {
        let key = std::fs::read_to_string(fixture("server.key")).unwrap();
        let body: String = key.lines().filter(|l| !l.starts_with("-----")).collect();
        let dir = tempfile::tempdir().unwrap();
        let mangled = dir.path().join("mangled.key");
        std::fs::write(&mangled, key.replace("-----END", "x-----END")).unwrap();
        let truncated = dir.path().join("truncated.key");
        std::fs::write(&truncated, &key[..key.len() / 2]).unwrap();
        for path in [&mangled, &truncated, &fixture("client-ann.key")] {
            let mut f = files("server.pem", "server.key");
            f.key = path.clone();
            let e = format!("{} {:?}", ServerTls::load(f.clone()).unwrap_err(), ServerTls::load(f).unwrap_err());
            for line in key.lines().filter(|l| !l.starts_with("-----")) {
                assert!(!e.contains(line), "{}", e);
            }
            assert!(!e.contains(&body[..16]), "{}", e);
        }
    }
}
