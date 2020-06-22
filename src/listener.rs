use std::io::{Error, Result};
use std::mem::{self, MaybeUninit};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::stream::Stream;
use tokio::net::{TcpListener, TcpStream};

fn get_original_addr<T: AsRawFd>(fd: &T) -> Result<SocketAddr> {
    let mut ipv6_addr = MaybeUninit::<libc::sockaddr_in6>::zeroed();
    let mut size = mem::size_of::<libc::sockaddr_in6>() as u32;

    unsafe {
        if libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_IPV6,
            libc::IP6T_SO_ORIGINAL_DST,
            ipv6_addr.as_mut_ptr() as _,
            &mut size as _,
        ) == 0
        {
            let ipv6_addr = ipv6_addr.assume_init();

            return Ok(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(ipv6_addr.sin6_addr.s6_addr),
                ipv6_addr.sin6_port.to_be(),
                ipv6_addr.sin6_flowinfo,
                ipv6_addr.sin6_scope_id,
            )));
        }
    }

    let mut ipv4_addr = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut size = mem::size_of::<libc::sockaddr_in>() as u32;

    unsafe {
        if libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_IP,
            libc::SO_ORIGINAL_DST,
            ipv4_addr.as_mut_ptr() as _,
            &mut size as _,
        ) < 0
        {
            Err(Error::last_os_error())
        } else {
            let ipv4_addr = ipv4_addr.assume_init();

            Ok(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::from(ipv4_addr.sin_addr.s_addr.to_be()),
                ipv4_addr.sin_port.to_be(),
            )))
        }
    }
}

pub struct Listener {
    inner: TcpListener,
}

impl From<TcpListener> for Listener {
    fn from(listener: TcpListener) -> Self {
        Self { inner: listener }
    }
}

impl Stream for Listener {
    type Item = Result<(TcpStream, SocketAddr)>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match futures_util::ready!(self.inner.poll_accept(cx)) {
            Err(err) => Poll::Ready(Some(Err(err))),
            Ok((stream, _)) => match get_original_addr(&stream) {
                Err(err) => Poll::Ready(Some(Err(err))),
                Ok(addr) => Poll::Ready(Some(Ok((stream, addr)))),
            },
        }
    }
}
