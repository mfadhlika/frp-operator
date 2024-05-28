mod context;
mod controllers;
mod error;
mod frpc;

use clap::Parser;
use log::info;

use frpc::config::{Auth, ClientConfig, WebServer};

use crate::frpc::config::Transport;

pub const OPERATOR_MANAGER: &str = "frp-operator";

#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long)]
    server_addr: String,
    #[arg(short, long)]
    server_port: u16,
    #[arg(short, long, default_value = "127.0.0.1")]
    webserver_addr: String,
    #[arg(short, long, default_value_t = 7400_u16)]
    webserver_port: u16,
    #[arg(short, long, env)]
    auth_token: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    info!("starting frp operator");

    let args = Args::parse();

    let cfg = ClientConfig {
        server_addr: args.server_addr,
        server_port: args.server_port,
        webserver: Some(WebServer {
            addr: Some(args.webserver_addr),
            port: args.webserver_port,
        }),
        auth: args.auth_token.map(|token| Auth {
            method: "token".to_string(),
            token: Some(token),
        }),
        includes: vec!["/etc/frp/proxy-*.toml".to_string()],
        transport: Some(Transport {
            protocol: Some("quic".to_string()),
        }),
        ..ClientConfig::default()
    };

    controllers::run(cfg).await?;

    Ok(())
}
