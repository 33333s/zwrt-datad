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
    #[arg(long, env = "ZWRT_DATAD_DIR", default_value = "/data/zwrt-datad")]
    data_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let raw: Vec<String> = std::env::args().collect();
    if raw.get(1).map(String::as_str) == Some("--neighbor-parse") {
        std::process::exit(neighbor::parse_cli(&raw[2..]));
    }
    if raw.get(1).map(String::as_str) == Some("--compare-state-shape") {
        match model::compare_state_shape(&raw[2..]) {
            Ok(value) => {
                println!("{}", serde_json::to_string(&value)?);
                return Ok(());
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(64);
            }
        }
    }
    let args = Args::parse();
    let interval = Duration::from_millis(args.interval.clamp(500, 5000));
    let _ = (
        &args.neighbor,
        &args.auth_token_file,
        &args.lan_bind,
        args.lan_port,
    );
    let token = match args.auth_token_file {
        Some(path) => std::fs::read_to_string(path)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty()),
        None => None,
    };
    let local_requires_auth = args.lan_bind.is_none() && token.is_some();
    let app = App::new(args.data_dir, interval, token).await?;
    if args.once {
        println!("{}", serde_json::to_string(&app.snapshot().await)?);
        return Ok(());
    }
    let addr: SocketAddr = format!("{}:{}", args.bind, args.port).parse()?;
    if let Some(lan_bind) = args.lan_bind {
        let lan_addr: SocketAddr = format!("{}:{}", lan_bind, args.lan_port).parse()?;
        tokio::try_join!(app.clone().serve(addr, false), app.serve(lan_addr, true))?;
        Ok(())
    } else {
        app.serve(addr, local_requires_auth).await
    }
}
