use std::convert::TryFrom;
use std::io::Error;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
use std::task::Poll;

use bytes::{Buf, BufMut, BytesMut};
use log::debug;
use mio::unix::EventedFd;
use mio::Ready;
use nix::fcntl;
use nix::fcntl::{FcntlArg, OFlag, SpliceFFlags};
use nix::unistd;
use tokio::io::{AsyncReadExt, AsyncWriteExt, PollEvented};
use tokio::net::TcpStream;

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
        Self { addr }
    }
}

#[async_trait::async_trait]
impl crate::Proxy for Proxy {
    async fn handle(&self, stream: TcpStream, addr: SocketAddr) -> IoResult<()> {
        debug!("target {}", addr);

        stream.set_nodelay(true)?;

        let local_stream_fd = stream.as_raw_fd();
        let local_stream_fd = unistd::dup(local_stream_fd).map_err(nix_error_to_io_error)?;

        set_non_block(local_stream_fd)?;

        debug!("set local tcp non block");

        let local_stream_fd = EventedFd(&local_stream_fd);
        drop(stream);

        let local_stream = Arc::new(PollEvented::new(local_stream_fd)?);

        let mut remote_stream = TcpStream::connect(&self.addr).await?;

        remote_stream.set_nodelay(true)?;

        let mut buf = if addr.is_ipv4() {
            BytesMut::with_capacity(4 + 4 + 2)
        } else {
            BytesMut::with_capacity(4 + 16 + 2)
        };

        buf.put_u8(VERSION);
        buf.put_u8(1);
        buf.put_u8(Auth::NoAuth.into());

        remote_stream.write_all(&buf).await?;
        remote_stream.flush().await?;

        // Safety: cap >= 4 + 4 + 2
        unsafe { buf.set_len(2) }

        remote_stream.read_exact(&mut buf).await?;

        if buf.get_u8() != VERSION {
            return Err(Error::new(
                ErrorKind::ConnectionAborted,
                format!("socks server version is {} not {}", buf[0], VERSION),
            ));
        }

        if buf.get_u8() != Auth::NoAuth.into() {
            return Err(Error::new(
                ErrorKind::ConnectionAborted,
                format!(
                    "socks server auth code is {} not {:?}",
                    buf[0],
                    Auth::NoAuth
                ),
            ));
        }

        buf.put_u8(VERSION);
        buf.put_u8(Cmd::Connect.into());
        buf.put_u8(0); // RSV

        if addr.is_ipv4() {
            buf.put_u8(1);
        } else {
            buf.put_u8(4);
        }

        match addr.ip() {
            IpAddr::V4(v4_addr) => buf.put_slice(&v4_addr.octets()),
            IpAddr::V6(v6_addr) => buf.put_slice(&v6_addr.octets()),
        }

        buf.put_u16(addr.port());

        remote_stream.write_all(&buf).await?;
        remote_stream.flush().await?;

        // Safety: cap >= 4 + 4 + 2
        unsafe { buf.set_len(4) }

        remote_stream.read_exact(&mut buf).await?;

        if buf.get_u8() != VERSION {
            return Err(Error::new(
                ErrorKind::ConnectionAborted,
                format!("socks server version is {} not {}", buf[0], VERSION),
            ));
        }

        let reply_code = ReplyCode::try_from(buf.get_u8())?;

        if reply_code != ReplyCode::Succeeded {
            return Err(Error::new(
                ErrorKind::ConnectionAborted,
                format!("socks connect failed {:?}", reply_code),
            ));
        }

        // ignore RSV
        buf.advance(1);

        match buf.get_u8() {
            1 => {
                // Safety: cap >= 4 + 4 + 2
                unsafe { buf.set_len(4 + 2) }

                remote_stream.read_exact(&mut buf).await?;
            }

            3 => {
                // Safety: cap >= 4 + 4 + 2
                unsafe { buf.set_len(1) }

                remote_stream.read_exact(&mut buf).await?;

                let domain_len = buf.get_u8() as usize;

                if domain_len + 2 > buf.capacity() {
                    buf.reserve(domain_len + 2 - buf.capacity());
                }

                unsafe { buf.set_len(domain_len + 2) }

                remote_stream.read_exact(&mut buf).await?;
            }

            4 => {
                if 16 + 2 > buf.capacity() {
                    buf.reserve(16 + 2 - buf.capacity());
                }

                unsafe { buf.set_len(16 + 2) }

                remote_stream.read_exact(&mut buf).await?;
            }

            r#type => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("unknown reply addr type {}", r#type),
                ));
            }
        }

        let remote_stream_fd = remote_stream.as_raw_fd();
        let remote_stream_fd = unistd::dup(remote_stream_fd).map_err(nix_error_to_io_error)?;

        set_non_block(remote_stream_fd)?;

        debug!("set remote tcp non block");

        let remote_stream_fd = EventedFd(&remote_stream_fd);
        drop(remote_stream);

        let remote_stream = Arc::new(PollEvented::new(remote_stream_fd)?);

        let local_to_remote = zero_copy(&local_stream, &remote_stream, PIPE_BUF_SIZE);
        let remote_to_local = zero_copy(&remote_stream, &local_stream, PIPE_BUF_SIZE);

        futures_util::try_join!(local_to_remote, remote_to_local)?;

        Ok(())
    }
}

pub async fn zero_copy(
    stream_in: &PollEvented<EventedFd<'_>>,
    stream_out: &PollEvented<EventedFd<'_>>,
    len: impl Into<Option<usize>>,
) -> IoResult<usize> {
    let len = len.into().unwrap_or_else(|| 4096);
    let flags =
        SpliceFFlags::SPLICE_F_NONBLOCK | SpliceFFlags::SPLICE_F_MORE | SpliceFFlags::SPLICE_F_MOVE;

    let (pr, pw) = os_pipe::pipe()?;
    let pr_fd = pr.as_raw_fd();
    let pw_fd = pw.as_raw_fd();

    set_non_block(pr_fd)?;

    debug!("set read pipe non-block");

    set_non_block(pw_fd)?;

    debug!("set write pipe non-block");

    let pr = PollEvented::new(EventedFd(&pr_fd))?;
    let pw = PollEvented::new(EventedFd(&pw_fd))?;

    let mut total = 0;

    loop {
        let mut n = loop {
            break match fcntl::splice(
                *stream_in.get_ref().0,
                None,
                *pw.get_ref().0,
                None,
                len,
                flags,
            )
            .map_err(nix_error_to_io_error)
            {
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    debug!("tcp to pipe would block");

                    futures_util::future::poll_fn(|cx| {
                        stream_in.poll_read_ready(cx, Ready::readable())
                    })
                    .await?;

                    futures_util::future::poll_fn(|cx| {
                        Poll::Ready(stream_in.clear_read_ready(cx, Ready::readable()))
                    })
                    .await?;

                    futures_util::future::poll_fn(|cx| pw.poll_write_ready(cx)).await?;

                    futures_util::future::poll_fn(|cx| Poll::Ready(pw.clear_write_ready(cx)))
                        .await?;

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
                    *pr.get_ref().0,
                    None,
                    *stream_out.get_ref().0,
                    None,
                    n,
                    flags,
                )
                .map_err(nix_error_to_io_error)
                {
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        debug!("pipe to tcp would block");

                        futures_util::future::poll_fn(|cx| {
                            pr.poll_read_ready(cx, Ready::readable())
                        })
                        .await?;

                        futures_util::future::poll_fn(|cx| {
                            Poll::Ready(pr.clear_read_ready(cx, Ready::readable()))
                        })
                        .await?;

                        futures_util::future::poll_fn(|cx| stream_out.poll_write_ready(cx)).await?;

                        futures_util::future::poll_fn(|cx| {
                            Poll::Ready(stream_out.clear_write_ready(cx))
                        })
                        .await?;

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

fn set_non_block(fd: RawFd) -> IoResult<()> {
    let flags = fcntl::fcntl(fd, FcntlArg::F_GETFL).map_err(nix_error_to_io_error)?;
    let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;

    fcntl::fcntl(fd, FcntlArg::F_SETFL(flags)).map_err(nix_error_to_io_error)?;

    Ok(())
}
