use std::{sync::Arc, time::Duration};

use futures_util::StreamExt;
use k8s_openapi::api::core::v1::{LoadBalancerIngress, LoadBalancerStatus, Service, ServiceStatus};
use kube::{
    api::{Patch, PatchParams},
    runtime::{controller::Action, finalizer, reflector, watcher, Controller, WatchStreamExt},
    Api, ResourceExt,
};
use log::{error, info, warn};

use crate::{
    context::Context,
    error::Error,
    frpc::{
        self,
        config::{Proxy, ProxyConfig},
    },
    OPERATOR_MANAGER,
};

pub const SERVICE_FINALIZER: &str = "frp-operator.io/service-finalizer";

pub async fn proxy_from_service(svc: &Service) -> Result<ProxyConfig, Error> {
    let svc_name = svc.name_any();
    let mut config = ProxyConfig {
        name: svc_name.clone(),
        proxies: vec![],
    };

    let ns = svc.namespace().clone().unwrap_or("default".to_string());

    for port in svc
        .spec
        .as_ref()
        .and_then(|spec| spec.ports.as_ref())
        .into_iter()
        .flatten()
    {
        let name = format!(
            "svc-{svc_name}-{}",
            port.name.clone().unwrap_or(port.port.to_string())
        );

        config.proxies.push(Proxy {
            name,
            type_: port
                .protocol
                .as_ref()
                .map(|protocol| protocol.to_lowercase())
                .unwrap_or("tcp".to_string()),
            local_ip: Some(format!("{svc_name}.{ns}.svc.cluster.local")),
            local_port: Some(port.port as u16),
            remote_port: Some(port.port as u16),
            ..Proxy::default()
        });
    }

    return Ok(config);
}

async fn reconcile(obj: Arc<Service>, ctx: Arc<Context>) -> Result<Action, Error> {
    if obj
        .spec
        .as_ref()
        .filter(|spec| spec.type_ == Some("LoadBalancer".to_string()))
        .is_none()
        || obj
            .spec
            .as_ref()
            .filter(|spec| spec.load_balancer_class == Some("frp".to_string()))
            .is_none()
    {
        return Ok(Action::requeue(Duration::from_secs(3600)));
    }

    let obj_name = obj.name_any().to_owned();
    let obj_ns = obj.namespace().clone().unwrap_or("default".to_string());

    let client = ctx.client.clone();

    let service_api: Api<Service> = Api::namespaced(client.clone(), &obj_ns);

    finalizer(&service_api, SERVICE_FINALIZER, obj, |event| async {
        match event {
            finalizer::Event::Apply(svc) => {
                let config = proxy_from_service(&svc).await?;
                frpc::write_config_proxy_to_file(config).await?;

                frpc::reload().await?;

                let mut svc = service_api.get_status(&obj_name).await?;
                svc.status = Some(ServiceStatus {
                    load_balancer: Some(LoadBalancerStatus {
                        ingress: Some(vec![LoadBalancerIngress {
                            // hostname: todo!(),
                            ip: frpc::read_config_from_file()
                                .await
                                .map(|config| config.server_addr)
                                .ok(),
                            ..LoadBalancerIngress::default()
                        }]),
                    }),
                    ..ServiceStatus::default()
                });

                service_api
                    .patch_status(
                        &obj_name,
                        &PatchParams::apply(OPERATOR_MANAGER),
                        &Patch::Merge(svc),
                    )
                    .await?;
            }
            finalizer::Event::Cleanup(svc) => {
                frpc::remove_config_proxy_file(&svc.name_any()).await?;

                frpc::reload().await?;
            }
        }

        return Ok(Action::requeue(Duration::from_secs(3600)));
    })
    .await
    .map_err(|err| Error::FinalizerError(Box::new(err)))
}

fn error_policy<K>(_obj: Arc<K>, err: &Error, _ctx: Arc<Context>) -> Action {
    error!("reason: {}", err);
    Action::requeue(Duration::from_secs(15))
}

pub async fn run(ctx: Arc<Context>) -> anyhow::Result<()> {
    let client = ctx.client.clone();

    let cfg = watcher::Config::default();
    let svc_api: Api<Service> = Api::all(client.clone());

    let (reader, writer) = reflector::store();
    let stream = reflector(writer, watcher(svc_api, cfg))
        .default_backoff()
        .touched_objects();

    Controller::for_stream(stream, reader)
        .shutdown_on_signal()
        .run(reconcile, error_policy, ctx.clone())
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("reconciled service {:?}", o),
                Err(e) => warn!("reconcile service failed: {:?}", e),
            }
        })
        .await;

    Ok(())
}
