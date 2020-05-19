use std::convert::TryFrom;
use std::io::Error;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::net::IpAddr;

use async_std::net::{SocketAddr, TcpStream};
use futures_util::io::{AsyncRead, AsyncWrite};
use futures_util::{AsyncReadExt, AsyncWriteExt};
use log::debug;

const VERSION: u8 = 5;

#[derive(Debug, Copy, Clone)]
enum Cmd {
    Connect,
}

impl From<Cmd> for u8 {
    fn from(cmd: Cmd) -> Self {
        match cmd {
            Cmd::Connect => 1,
        }
    }
}

#[derive(Debug, Copy, Clone)]
enum Auth {
    NoAuth,
}

impl From<Auth> for u8 {
    fn from(auth: Auth) -> Self {
        match auth {
            Auth::NoAuth => 0,
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum ReplyCode {
    Succeeded = 0,
    ServerFail = 1,
    ConnectionNotAllowed = 2,
    NetWorkUnreachable = 3,
    HostUnreachable = 4,
    ConnectionRefused = 5,
    TTLExpired = 6,
    CmdNotSupport = 7,
    AddressTypeNotSupport = 8,
    Other = 9,
}

impl TryFrom<u8> for ReplyCode {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(ReplyCode::Succeeded),
            1 => Ok(ReplyCode::ServerFail),
            2 => Ok(ReplyCode::ConnectionNotAllowed),
            3 => Ok(ReplyCode::NetWorkUnreachable),
            4 => Ok(ReplyCode::HostUnreachable),
            5 => Ok(ReplyCode::ConnectionRefused),
            6 => Ok(ReplyCode::TTLExpired),
            7 => Ok(ReplyCode::CmdNotSupport),
            8 => Ok(ReplyCode::AddressTypeNotSupport),
            9 => Ok(ReplyCode::Other),

            _ => Err(Error::new(
                ErrorKind::Other,
                format!("unknown reply code {}", value),
            )),
        }
    }
}

pub struct Proxy {
    addr: SocketAddr,
}

impl Proxy {
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr }
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
        debug!("target {}", addr);

        let mut stream = TcpStream::connect(&self.addr).await?;

        let mut buf = if addr.is_ipv4() {
            vec![0; 4 + 4 + 2]
        } else {
            vec![0; 4 + 16 + 2]
        };

        buf[0] = VERSION;
        buf[1] = 1;

        stream.write_all(&buf[..3]).await?;

        stream.read_exact(&mut buf[..2]).await?;

        if buf[0] != VERSION {
            return Err(Error::new(
                ErrorKind::ConnectionAborted,
                format!("socks server version is {} not {}", buf[0], VERSION),
            ));
        }

        if buf[1] != Auth::NoAuth.into() {
            return Err(Error::new(
                ErrorKind::ConnectionAborted,
                format!(
                    "socks server auth code is {} not {:?}",
                    buf[0],
                    Auth::NoAuth
                ),
            ));
        }

        buf[0] = VERSION;
        buf[1] = Cmd::Connect.into();
        buf[2] = 0; // RSV

        buf[3] = if addr.is_ipv4() { 1 } else { 4 }; // addr type

        match addr.ip() {
            IpAddr::V4(v4_addr) => {
                buf[4..8].copy_from_slice(&v4_addr.octets());
                buf[8..10].copy_from_slice(&addr.port().to_be_bytes())
            }

            IpAddr::V6(v6_addr) => {
                buf[4..20].copy_from_slice(&v6_addr.octets());
                buf[20..22].copy_from_slice(&addr.port().to_be_bytes());
            }
        }

        stream.write_all(&buf).await?;

        stream.read_exact(&mut buf[..4]).await?;

        if buf[0] != VERSION {
            return Err(Error::new(
                ErrorKind::ConnectionAborted,
                format!("socks server version is {} not {}", buf[0], VERSION),
            ));
        }

        let reply_code = ReplyCode::try_from(buf[1])?;

        if reply_code != ReplyCode::Succeeded {
            return Err(Error::new(
                ErrorKind::ConnectionAborted,
                format!("socks connect failed {:?}", reply_code),
            ));
        }

        match buf[3] {
            // buf size is always enough
            1 => stream.read_exact(&mut buf[..4 + 2]).await?,

            3 => {
                stream.read_exact(&mut buf[..1]).await?;

                let domain_len = buf[0] as usize;

                // reuse buffer
                if domain_len + 2 < buf.len() {
                    stream.read_exact(&mut buf[..domain_len + 2]).await?;
                } else {
                    stream.read_exact(&mut vec![0; domain_len + 2]).await?;
                }
            }

            4 => {
                if buf.len() < 16 + 2 {
                    buf = vec![0; 16 + 2];
                }

                stream.read_exact(&mut buf).await?;
            }

            r#type => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("unknown reply addr type {}", r#type),
                ));
            }
        }

        let mut read_stream = &stream;
        let mut write_stream = &stream;

        let server_to_client = async_std::io::copy(&mut read_stream, &mut writer);
        let client_to_server = async_std::io::copy(&mut reader, &mut write_stream);

        futures_util::try_join!(server_to_client, client_to_server)?;

        Ok(())
    }
}
