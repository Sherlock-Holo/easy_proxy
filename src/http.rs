use std::future::Future;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::uri::Scheme;
use hyper::{Body, Client, Request, upgrade, Uri};
use hyper::client::connect::{Connected, Connection};
use hyper::service::Service;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

#[derive(Debug)]
pub enum AsyncConnection {
    TCP(TcpStream),
    TLS(Box<TlsStream<TcpStream>>),
}

impl Connection for AsyncConnection {
    fn connected(&self) -> Connected {
        let connected = Connected::new();

        match self {
            AsyncConnection::TCP(stream) => {
                if let Ok(remote_addr) = stream.peer_addr() {
                    connected.extra(remote_addr)
                } else {
                    connected
                }
            }

            AsyncConnection::TLS(stream) => {
                if let Ok(remote_addr) = stream.get_ref().0.peer_addr() {
                    connected.extra(remote_addr)
                } else {
                    connected
                }
            }
        }
    }
}

impl AsyncRead for AsyncConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<IoResult<()>> {
        match self.get_mut() {
            AsyncConnection::TCP(stream) => Pin::new(stream).poll_read(cx, buf),
            AsyncConnection::TLS(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for AsyncConnection {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<IoResult<usize>> {
        match self.get_mut() {
            AsyncConnection::TCP(stream) => Pin::new(stream).poll_write(cx, buf),
            AsyncConnection::TLS(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        match self.get_mut() {
            AsyncConnection::TCP(stream) => Pin::new(stream).poll_flush(cx),
            AsyncConnection::TLS(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        match self.get_mut() {
            AsyncConnection::TCP(stream) => Pin::new(stream).poll_shutdown(cx),
            AsyncConnection::TLS(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

#[derive(Clone)]
struct InnerProxy {
    host: String,
    port: u16,
    tls: Option<TlsConnector>,
}

impl InnerProxy {
    fn new(addr: Uri) -> IoResult<Self> {
        let (tls, mut port) = match addr.scheme() {
            Some(scheme) => {
                if scheme == &Scheme::HTTP {
                    (None, 80)
                } else if scheme == &Scheme::HTTPS {
                    let tls = Some(TlsConnector::from(Arc::new(
                        tokio_rustls::rustls::ClientConfig::default(),
                    )));

                    (tls, 443)
                } else {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!("unknown scheme {}", scheme),
                    ));
                }
            }

            None => (None, 80),
        };

        port = addr.port_u16().unwrap_or(port);

        let host = addr
            .host()
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "unknown port"))?
            .to_string();

        Ok(Self { host, port, tls })
    }
}

type ResponseFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

impl Service<Uri> for InnerProxy {
    type Response = AsyncConnection;
    type Error = Error;
    type Future = ResponseFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: Uri) -> Self::Future {
        let host = self.host.to_string();
        let tls = self.tls.clone();

        let addr = format!("{}:{}", host, self.port);

        Box::pin(async move {
            let stream = TcpStream::connect(addr).await?;

            if let Some(tls) = tls {
                let dns_name_ref = tokio_rustls::webpki::DNSNameRef::try_from_ascii_str(&host)
                    .map_err(|err| Error::new(ErrorKind::Other, err))?;

                Ok(AsyncConnection::TLS(Box::new(
                    tls.connect(dns_name_ref, stream).await?,
                )))
            } else {
                Ok(AsyncConnection::TCP(stream))
            }
        })
    }
}

pub struct Proxy {
    client: Client<InnerProxy>,
}

impl Proxy {
    pub fn new(uri: Uri) -> IoResult<Self> {
        let inner_proxy = InnerProxy::new(uri)?;

        let client = Client::builder().build(inner_proxy);

        Ok(Self { client })
    }
}

#[async_trait::async_trait]
impl crate::Proxy for Proxy {
    async fn handle(&self, mut stream: TcpStream, addr: SocketAddr) -> IoResult<()> {
        let addr: Uri = match addr.to_string().parse() {
            Err(err) => return Err(Error::new(ErrorKind::InvalidInput, err)),
            Ok(addr) => addr,
        };

        let req = match Request::connect(addr).body(Body::empty()) {
            Err(err) => return Err(Error::new(ErrorKind::InvalidInput, err)),
            Ok(req) => req,
        };

        let resp = match self.client.request(req).await {
            Err(err) => return Err(Error::new(ErrorKind::Other, err)),
            Ok(resp) => resp,
        };

        let upgraded = match upgrade::on(resp).await {
            Err(err) => return Err(Error::new(ErrorKind::Other, err)),
            Ok(upgraded) => upgraded,
        };

        let (mut server_reader, mut server_writer) = tokio::io::split(upgraded);

        let (mut read_stream, mut write_stream) = stream.split();

        let client_to_server = tokio::io::copy(&mut read_stream, &mut server_writer);
        let server_to_client = tokio::io::copy(&mut server_reader, &mut write_stream);

        futures_util::try_join!(client_to_server, server_to_client)?;

        Ok(())
    }
}
