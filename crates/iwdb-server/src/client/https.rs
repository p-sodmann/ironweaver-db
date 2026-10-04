//! The REST client's connections: TCP, and over it TLS for `https://`
//! endpoints (step 15b), with rustls. hyper-util's `HttpConnector` makes
//! the TCP connection; this adds the handshake.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection, HttpConnector};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tower_service::Service;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Connects plaintext, or with `tls` over TLS.
#[derive(Clone)]
pub(crate) struct Connector {
    http: HttpConnector,
    tls: Option<tokio_rustls::TlsConnector>,
}

impl Connector {
    pub(crate) fn new(tls: Option<Arc<rustls::ClientConfig>>) -> Self {
        let mut http = HttpConnector::new();
        http.set_nodelay(true);
        // The scheme is ours to check (https:// is TLS here)
        http.enforce_http(false);
        Connector { http, tls: tls.map(tokio_rustls::TlsConnector::from) }
    }
}

impl Service<Uri> for Connector {
    type Response = Conn;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Conn, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        self.http.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let host = uri.host().unwrap_or_default().trim_start_matches('[').trim_end_matches(']').to_owned();
        let connecting = self.http.call(uri);
        let tls = self.tls.clone();
        Box::pin(async move {
            let tcp = connecting.await?;
            let Some(tls) = tls else { return Ok(Conn::Plain(tcp)) };
            let name = rustls_pki_types::ServerName::try_from(host)?;
            let stream = tls.connect(name, tcp.into_inner()).await?;
            Ok(Conn::Tls(Box::new(TokioIo::new(stream))))
        })
    }
}

/// A connection of [`Connector`].
pub(crate) enum Conn {
    Plain(TokioIo<TcpStream>),
    Tls(Box<TokioIo<tokio_rustls::client::TlsStream<TcpStream>>>),
}

impl Connection for Conn {
    fn connected(&self) -> Connected {
        match self {
            Conn::Plain(s) => s.connected(),
            Conn::Tls(s) => s.inner().get_ref().0.connected(),
        }
    }
}

impl hyper::rt::Read for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Conn::Tls(s) => Pin::new(&mut **s).poll_read(cx, buf),
        }
    }
}

impl hyper::rt::Write for Conn {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Conn::Tls(s) => Pin::new(&mut **s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_flush(cx),
            Conn::Tls(s) => Pin::new(&mut **s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Conn::Tls(s) => Pin::new(&mut **s).poll_shutdown(cx),
        }
    }
}
