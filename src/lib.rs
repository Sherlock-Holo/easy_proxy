use std::io::Result;
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::sync::Arc;

use async_std::net::TcpListener;
use async_std::task;
use futures_util::io::AsyncRead;
use futures_util::io::AsyncWrite;
use futures_util::StreamExt;
use log::{info, warn, LevelFilter};
use structopt::StructOpt;

mod http;
mod listener;
mod socks;

#[async_trait::async_trait]
pub trait Proxy {
    async fn handle<R, W>(&self, reader: R, writer: W, addr: SocketAddr) -> Result<()>
    where
        R: AsyncRead + Send + Unpin,
        W: AsyncWrite + Send + Unpin;
}

mod compat {
    use std::io::Result;
    use std::pin::Pin;
    use std::task::Poll;

    use futures_util::io::AsyncRead;
    use futures_util::io::AsyncWrite;
    use futures_util::task::Context;
    use tokio::io::AsyncRead as TokioRead;
    use tokio::io::AsyncWrite as TokioWrite;

    pub struct Reader<T>(pub T);

    impl<T: TokioRead + Unpin> AsyncRead for Reader<T> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<Result<usize>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    pub struct Writer<T>(pub T);

    impl<T: TokioWrite + Unpin> AsyncWrite for Writer<T> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }
}

#[derive(Debug, StructOpt)]
struct Opt {
    #[structopt(short, long, help = "proxy listen address, such as `127.0.0.1:1888`")]
    listen_addr: String,

    #[structopt(short, long, help = "enable debug log")]
    debug: bool,

    #[structopt(subcommand)]
    cmd: SubCommand,
}

#[derive(Debug, StructOpt)]
enum SubCommand {
    /// specify http proxy server
    Http {
        #[structopt(
            short,
            long,
            help = "http proxy address, such as `http://127.0.0.1:8080`"
        )]
        addr: String,
    },

    /// specify socks proxy server
    Socks {
        #[structopt(short, long, help = "socks5 address, such as `127.0.0.1:1080`")]
        addr: String,
    },
}

pub async fn run() -> Result<()> {
    let opt: Opt = Opt::from_args();

    log_init(opt.debug);

    let tcp_listener = TcpListener::bind(&opt.listen_addr).await?;

    let listener = listener::Listener::from(tcp_listener);

    info!("listen on {}", opt.listen_addr);

    match opt.cmd {
        SubCommand::Http { addr } => {
            let addr = addr
                .parse()
                .map_err(|err| Error::new(ErrorKind::InvalidInput, err))?;

            info!("http proxy mode, http proxy server {}", addr);

            let proxy = Arc::new(http::Proxy::new(addr)?);

            handle(proxy, listener).await;
        }

        SubCommand::Socks { addr } => {
            let addr = addr
                .parse()
                .map_err(|err| Error::new(ErrorKind::InvalidInput, err))?;

            info!("socks proxy mode, socks proxy server {}", addr);

            let proxy = Arc::new(socks::Proxy::new(addr));

            handle(proxy, listener).await;
        }
    };

    Ok(())
}

async fn handle<T: 'static + Proxy + Send + Sync>(proxy: Arc<T>, mut listener: listener::Listener) {
    while let Some(result) = listener.next().await {
        let (stream, target_addr) = match result {
            Err(err) => {
                warn!("accept failed {}", err);

                continue;
            }

            Ok(result) => result,
        };

        let proxy = proxy.clone();

        task::spawn(async move {
            let mut reader = &stream;
            let mut writer = &stream;

            if let Err(err) = proxy.handle(&mut reader, &mut writer, target_addr).await {
                warn!("proxy failed {}", err);
            }
        });
    }
}

pub fn log_init(debug: bool) {
    let mut builder = pretty_env_logger::formatted_timed_builder();

    if debug {
        builder.filter_level(LevelFilter::Debug);
    } else {
        builder.filter_level(LevelFilter::Info);
    }

    builder.init();
}
