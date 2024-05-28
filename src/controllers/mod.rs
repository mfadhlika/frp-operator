use std::sync::Arc;

use crate::{
    context::Context,
    error::Error,
    frpc::{self, config::ClientConfig},
};

pub mod ingress;
pub mod service;

pub async fn run(config: ClientConfig) -> Result<(), Error> {
    let client = kube::Client::try_default().await?;

    let ctx = Arc::new(Context { client });

    let frpc_fut = frpc::run(config);

    let ingress_fut = ingress::run(ctx.clone());

    let service_fut = service::run(ctx.clone());

    let _ = futures_util::join!(frpc_fut, ingress_fut, service_fut);

    Ok(())
}
