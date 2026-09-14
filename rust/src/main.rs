mod command;
mod model;
mod server;
mod state;

use anyhow::Result;
use clap::Parser;
use server::App;
use std::{net::SocketAddr, path::PathBuf, time::Duration};

#[derive(Parser, Debug)]
#[command(name = "zwrt-datad", version = env!("CARGO_PKG_VERSION"))]
struct Args {
    #[arg(long)]
    once: bool,
    #[arg(short = 'i', default_value_t = 1000)]
    interval: u64,
    #[arg(short = 'b', long = "bind", default_value = "127.0.0.1")]
    bind: String,
    #[arg(short = 'p', long = "port", default_value_t = 9460)]
    port: u16,
    #[arg(long, default_value = "/data/zwrt-datad")]
    data_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let interval = Duration::from_millis(args.interval.clamp(500, 5000));
    let app = App::new(args.data_dir, interval).await?;
    if args.once {
        println!("{}", serde_json::to_string(&app.snapshot().await)?);
        return Ok(());
    }
    let addr: SocketAddr = format!("{}:{}", args.bind, args.port).parse()?;
    app.serve(addr).await
}
