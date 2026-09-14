mod command;
mod model;
mod neighbor;
mod server;
mod state;

use anyhow::Result;
use clap::Parser;
use server::App;
use std::{net::SocketAddr, path::PathBuf, time::Duration};

#[derive(Parser, Debug)]
#[command(name = "zwrt-datad", version = env!("DATAD_VERSION"))]
struct Args {
    #[arg(long)]
    once: bool,
    #[arg(long)]
    neighbor: bool,
    #[arg(long)]
    auth_token_file: Option<PathBuf>,
    #[arg(long)]
    lan_bind: Option<String>,
    #[arg(long, default_value_t = 9461)]
    lan_port: u16,
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
    let raw: Vec<String> = std::env::args().collect();
    if raw.get(1).map(String::as_str) == Some("--neighbor-parse") {
        std::process::exit(neighbor::parse_cli(&raw[2..]));
    }
    let args = Args::parse();
    let interval = Duration::from_millis(args.interval.clamp(500, 5000));
    let _ = (
        &args.neighbor,
        &args.auth_token_file,
        &args.lan_bind,
        args.lan_port,
    );
    let app = App::new(args.data_dir, interval).await?;
    if args.once {
        println!("{}", serde_json::to_string(&app.snapshot().await)?);
        return Ok(());
    }
    let addr: SocketAddr = format!("{}:{}", args.bind, args.port).parse()?;
    app.serve(addr).await
}
