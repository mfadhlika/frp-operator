mod config;
mod context;
mod controllers;
mod error;

use log::info;

pub const OPERATOR_MANAGER: &str = "frp-operator";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    info!("starting frp operator");

    controllers::run().await?;

    Ok(())
}
