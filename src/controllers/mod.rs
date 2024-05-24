use std::sync::Arc;

use crate::{context::Context, error::Error};

pub mod client;
pub mod ingress;

pub async fn run() -> Result<(), Error> {
    let client = kube::Client::try_default().await?;

    let ctx = Arc::new(Context { client });

    let client_fut = client::run(ctx.clone());

    let ingress_fut = ingress::run(ctx.clone());

    let _ = futures_util::join!(client_fut, ingress_fut);

    Ok(())
}
