use std::future::Future;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_std::net::TcpStream;
use async_tls::client::TlsStream;
use async_tls::TlsConnector;
use futures_util::io::{AsyncRead, AsyncWrite};
use hyper::client::connect::{Connected, Connection};
use hyper::rt::Executor;
use hyper::service::Service;
use hyper::{Body, Client, Request, Uri};
use tokio::io::AsyncRead as TokioRead;
use tokio::io::AsyncWrite as TokioWrite;

use crate::compat::*;

#[derive(Debug, Default)]
pub struct AsyncExecutor;

impl Executor<Pin<Box<dyn Future<Output = ()> + Send>>> for AsyncExecutor {
    fn execute(&self, fut: Pin<Box<dyn Future<Output = ()> + Send>>) {
        async_std::task::spawn(fut);
    }
}

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
                if let Ok(remote_addr) = stream.get_ref().peer_addr() {
                    connected.extra(remote_addr)
                } else {
                    connected
                }
            }
        }
    }
}

impl TokioRead for AsyncConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<IoResult<usize>> {
        match self.get_mut() {
            AsyncConnection::TCP(stream) => Pin::new(stream).poll_read(cx, buf),
            AsyncConnection::TLS(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl TokioWrite for AsyncConnection {
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
            AsyncConnection::TCP(stream) => Pin::new(stream).poll_close(cx),
            AsyncConnection::TLS(stream) => Pin::new(stream).poll_close(cx),
        }
    }
}

#[derive(Debug, Default, Clone)]
struct InnerProxy {
    host: String,
    port: u16,
    tls: bool,
}

impl InnerProxy {
    fn new(addr: Uri) -> IoResult<Self> {
        let tls = if let Some(scheme) = addr.scheme_str() {
            scheme == "https"
        } else {
            false
        };

        let port = if let Some(port) = addr.port_u16() {
            port
        } else if let Some(scheme) = addr.scheme_str() {
            match scheme {
                "http" => 80,
                "https" => 443,
                _ => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "unknown scheme or port",
                    ))
                }
            }
        } else {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "unknown scheme or port",
            ));
        };

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
        let tls = self.tls;

        let addr = format!("{}:{}", host, self.port);

        Box::pin(async move {
            let stream = TcpStream::connect(addr).await?;

            if tls {
                Ok(AsyncConnection::TLS(Box::new(
                    TlsConnector::new().connect(host, stream).await?,
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

        let client = Client::builder()
            .executor(AsyncExecutor::default())
            .build(inner_proxy);

        Ok(Self { client })
    }
}

#[async_trait::async_trait]
impl crate::Proxy for Proxy {
    async fn handle<R, W>(&self, mut reader: R, mut writer: W, addr: SocketAddr) -> IoResult<()>
    where
        R: AsyncRead + Send + Unpin,
        W: AsyncWrite + Send + Unpin,
        Self: Sized,
    {
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

        let upgraded = match resp.into_body().on_upgrade().await {
            Err(err) => return Err(Error::new(ErrorKind::Other, err)),
            Ok(upgraded) => upgraded,
        };

        let (server_reader, server_writer) = tokio::io::split(upgraded);

        let mut server_reader = Reader(server_reader);
        let mut server_writer = Writer(server_writer);

        let client_to_server = async_std::io::copy(&mut reader, &mut server_writer);

        let server_to_client = async_std::io::copy(&mut server_reader, &mut writer);

        futures_util::try_join!(client_to_server, server_to_client)?;

        Ok(())
    }
}
