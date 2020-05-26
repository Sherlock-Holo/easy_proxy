use std::convert::TryFrom;
use std::io::Error;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::net::IpAddr;
use std::os::unix::io::AsRawFd;

use async_std::net::{SocketAddr, TcpStream};
use futures_util::{AsyncReadExt, AsyncWriteExt};
use log::debug;
use nix::fcntl;
use nix::fcntl::SpliceFFlags;
use smol::Async;

const VERSION: u8 = 5;
const PIPE_BUF_SIZE: usize = 65535;

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
        // TODO remove it when smol merge my PRs
        for _ in 0..num_cpus::get().min(1) * 2 {
            std::thread::spawn(|| smol::run(futures_util::future::pending::<()>()));
        }

        Self { addr }
    }
}

#[async_trait::async_trait]
impl crate::Proxy for Proxy {
    async fn handle(&self, stream: &TcpStream, addr: SocketAddr) -> IoResult<()> {
        debug!("target {}", addr);

        let local_stream = Async::new(stream.clone())?;

        let mut remote_stream = TcpStream::connect(&self.addr).await?;

        let mut buf = if addr.is_ipv4() {
            vec![0; 4 + 4 + 2]
        } else {
            vec![0; 4 + 16 + 2]
        };

        buf[0] = VERSION;
        buf[1] = 1;

        remote_stream.write_all(&buf[..3]).await?;

        remote_stream.read_exact(&mut buf[..2]).await?;

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

        remote_stream.write_all(&buf).await?;

        remote_stream.read_exact(&mut buf[..4]).await?;

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
            1 => remote_stream.read_exact(&mut buf[..4 + 2]).await?,

            3 => {
                remote_stream.read_exact(&mut buf[..1]).await?;

                let domain_len = buf[0] as usize;

                // reuse buffer
                if domain_len + 2 < buf.len() {
                    remote_stream.read_exact(&mut buf[..domain_len + 2]).await?;
                } else {
                    remote_stream
                        .read_exact(&mut vec![0; domain_len + 2])
                        .await?;
                }
            }

            4 => {
                if buf.len() < 16 + 2 {
                    buf = vec![0; 16 + 2];
                }

                remote_stream.read_exact(&mut buf).await?;
            }

            r#type => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("unknown reply addr type {}", r#type),
                ));
            }
        }

        let remote_stream = Async::new(remote_stream)?;

        let local_to_remote = zero_copy(&local_stream, &remote_stream, PIPE_BUF_SIZE);
        let remote_to_local = zero_copy(&remote_stream, &local_stream, PIPE_BUF_SIZE);

        futures_util::try_join!(local_to_remote, remote_to_local)?;

        Ok(())
    }
}

pub async fn zero_copy(
    stream_in: &Async<TcpStream>,
    stream_out: &Async<TcpStream>,
    len: impl Into<Option<usize>>,
) -> IoResult<usize> {
    let len = len.into().unwrap_or_else(|| 4096);
    let flags =
        SpliceFFlags::SPLICE_F_NONBLOCK | SpliceFFlags::SPLICE_F_MORE | SpliceFFlags::SPLICE_F_MOVE;

    let (pr, pw) = os_pipe::pipe()?;
    let pr = Async::new(pr)?;
    let pw = Async::new(pw)?;

    let mut total = 0;

    loop {
        let mut n = loop {
            break match fcntl::splice(
                stream_in.as_raw_fd(),
                None,
                pw.as_raw_fd(),
                None,
                len,
                flags,
            )
            .map_err(nix_error_to_io_error)
            {
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    stream_in.readable().await?;
                    pw.writable().await?;

                    continue;
                }

                res => res?,
            };
        };

        if n == 0 {
            return Ok(total);
        }

        total += n;

        while n > 0 {
            let written = loop {
                break match fcntl::splice(
                    pr.as_raw_fd(),
                    None,
                    stream_out.as_raw_fd(),
                    None,
                    n,
                    flags,
                )
                .map_err(nix_error_to_io_error)
                {
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        pr.readable().await?;
                        stream_out.writable().await?;

                        continue;
                    }

                    res => res?,
                };
            };

            n -= written;
        }
    }
}

fn nix_error_to_io_error(err: nix::Error) -> Error {
    match err {
        nix::Error::InvalidUtf8 | nix::Error::InvalidPath => Error::from(ErrorKind::InvalidInput),
        nix::Error::UnsupportedOperation => Error::from_raw_os_error(libc::ENOTSUP),
        nix::Error::Sys(errno) => Error::from(errno),
    }
}
