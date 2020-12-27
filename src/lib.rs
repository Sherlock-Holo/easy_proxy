use std::io::{Error, ErrorKind};
use std::io::Result;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::StreamExt;
use log::{info, Level, warn};
use simple_logger::SimpleLogger;
use structopt::clap::AppSettings::*;
use structopt::StructOpt;
use tokio::net::{TcpListener, TcpStream};

mod http;
mod listener;
mod socks;

#[async_trait::async_trait]
pub trait Proxy {
    async fn handle(&self, stream: TcpStream, addr: SocketAddr) -> Result<()>;
}

#[derive(Debug, StructOpt)]
#[structopt(setting(ColorAuto), setting(ColoredHelp))]
struct Opt {
    #[structopt(short, long, help = "proxy listen address, such as `127.0.0.1:1888`")]
    listen_addr: String,

    #[structopt(short, long, help = "enable debug log")]
    debug: bool,

    #[structopt(subcommand)]
    cmd: SubCommand,
}

#[derive(Debug, StructOpt)]
#[structopt(setting(ColorAuto), setting(ColoredHelp))]
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

        tokio::spawn(async move {
            if let Err(err) = proxy.handle(stream, target_addr).await {
                warn!("proxy failed {}", err);
            }
        });
    }
}

pub fn log_init(debug: bool) {
    if debug {
        SimpleLogger::new()
            .with_level(Level::Debug.to_level_filter())
            .init()
            .unwrap()
    } else {
        SimpleLogger::new()
            .with_level(Level::Info.to_level_filter())
            .init()
            .unwrap()
    }
}
