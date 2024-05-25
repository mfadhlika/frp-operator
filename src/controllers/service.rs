use std::{sync::Arc, time::Duration};

use futures_util::StreamExt;
use json_patch::{PatchOperation, RemoveOperation};
use k8s_openapi::{
    api::{
        apps::v1::Deployment,
        core::v1::{
            ConfigMap, LoadBalancerIngress, LoadBalancerStatus, Pod, Service, ServiceStatus,
        },
    },
    apimachinery::pkg::util::intstr::IntOrString,
};
use kube::{
    api::{DeleteParams, ListParams, Patch, PatchParams},
    runtime::{controller::Action, finalizer, reflector, watcher, Controller, WatchStreamExt},
    Api, ResourceExt,
};
use log::{error, info, warn};

use super::{client::Client, configmap_from_proxy, patch_deployment, volumes_from_proxy};
use crate::{
    config::{LoadBalancer, Proxy, ProxyConfig},
    context::Context,
    error::Error,
    OPERATOR_MANAGER,
};
use anyhow::anyhow;

pub const SERVICE_FINALIZER: &str = "frp-operator.io/service-finalizer";

pub async fn proxy_from_service(
    svc: &Service,
    client: &kube::Client,
) -> Result<ProxyConfig, Error> {
    let mut proxy_config = ProxyConfig {
        name: svc.metadata.name.as_ref().unwrap().to_owned(),
        ..ProxyConfig::default()
    };

    let ns = svc.namespace().clone().unwrap_or("default".to_string());
    let pod_api: Api<Pod> = Api::namespaced(client.clone(), &ns);

    let mut lp = ListParams::default();
    if let Some(selector) = svc.spec.as_ref().and_then(|spec| spec.selector.as_ref()) {
        let label_selector = selector
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<String>>()
            .join(",");
        lp = lp.labels(&label_selector);
    }

    let pods = pod_api.list(&lp).await?;

    for port in svc
        .spec
        .as_ref()
        .and_then(|spec| spec.ports.as_ref())
        .into_iter()
        .flatten()
    {
        if port
            .protocol
            .as_ref()
            .filter(|protocol| protocol.eq_ignore_ascii_case("tcp"))
            .is_none()
        {
            warn!("only tcp is supported");
            continue;
        }

        let group = format!(
            "{}-{}",
            svc.name_any(),
            port.name.clone().unwrap_or(port.port.to_string())
        );
        for (index, pod) in pods.items.iter().enumerate() {
            let Some(pod_ip) = pod
                .status
                .as_ref()
                .and_then(|status| status.pod_ip.as_ref())
            else {
                continue;
            };

            let local_port = match &port.target_port {
                Some(IntOrString::Int(port)) => *port as u16,
                Some(IntOrString::String(name)) => {
                    let Some(port) = pod
                        .spec
                        .clone()
                        .map(|spec| spec.containers)
                        .into_iter()
                        .flatten()
                        .filter_map(|c| c.ports)
                        .flatten()
                        .find(|port| port.name == Some(name.to_string()))
                    else {
                        warn!("port {name} not found");
                        continue;
                    };

                    port.container_port as u16
                }
                None => {
                    continue;
                }
            };
            proxy_config.proxies.push(Proxy {
                name: format!("{group}-{index}"),
                type_: "tcp".to_string(),
                local_ip: Some(pod_ip.clone()),
                local_port: Some(local_port),
                remote_port: Some(port.port as u16),
                load_balancer: Some(LoadBalancer {
                    group: group.clone(),
                    group_key: group.clone(),
                }),
                ..Proxy::default()
            });
        }
    }

    return Ok(proxy_config);
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
        return Ok(Action::requeue(Duration::from_secs(60)));
    }

    let client = ctx.client.clone();

    let cli_api: Api<Client> = Api::all(client.clone());

    let clis = cli_api.list(&ListParams::default().timeout(30)).await?;
    let cli = clis
        .iter()
        .nth(0)
        .ok_or_else(|| anyhow!("no client found"))?;

    let cli_ns = cli.namespace().clone().unwrap_or("default".to_string());
    let configmap_api: Api<ConfigMap> = Api::namespaced(client.clone(), &cli_ns);

    let obj_name = obj.name_any().to_owned();
    let obj_ns = obj.namespace().clone().unwrap_or("default".to_string());

    let config_name = format!("config-proxy-{}", obj.name_any());
    let cm_name = format!("frpc-{config_name}");

    let service_api: Api<Service> = Api::namespaced(client.clone(), &obj_ns);
    let deploy_api: Api<Deployment> = Api::namespaced(client.clone(), &cli_ns);

    let dep: Deployment = deploy_api.get("frpc").await?;

    finalizer(&service_api, SERVICE_FINALIZER, obj, |event| async {
        match event {
            finalizer::Event::Apply(ing) => {
                let oref = dep.owner_references().get(0).unwrap();

                let proxy = proxy_from_service(&ing, &client).await?;

                let cm = configmap_from_proxy(oref, &proxy, &cli_ns)?;
                configmap_api
                    .patch(
                        &cm_name,
                        &PatchParams::apply(OPERATOR_MANAGER),
                        &Patch::Apply(&cm),
                    )
                    .await?;

                let spec = dep
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.template.spec.as_ref())
                    .unwrap();

                let mut volumes = spec.volumes.clone().unwrap_or(vec![]);
                let mut volume_mounts = spec
                    .containers
                    .get(0)
                    .and_then(|c| c.volume_mounts.clone())
                    .unwrap_or(vec![]);

                volumes_from_proxy(&proxy, &mut volumes, &mut volume_mounts);

                patch_deployment(&deploy_api, &volumes, &volume_mounts).await?;

                let mut svc = service_api.get_status(&obj_name).await?;
                svc.status = Some(ServiceStatus {
                    load_balancer: Some(LoadBalancerStatus {
                        ingress: Some(vec![LoadBalancerIngress {
                            // hostname: todo!(),
                            ip: Some(cli.spec.server_addr.to_owned()),
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
            finalizer::Event::Cleanup(_ing) => {
                // look for orphaned resources
                let pod_spec = dep
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.template.spec.as_ref())
                    .unwrap();

                let mut patches = vec![];
                // find volume related to the ingress being cleanup
                if let Some(index) = pod_spec
                    .containers
                    .get(0)
                    .and_then(|c| c.volume_mounts.as_ref())
                    .and_then(|vms| vms.iter().position(|vol| vol.name == config_name))
                {
                    patches.push(PatchOperation::Remove(RemoveOperation {
                        path: format!("/spec/template/spec/volumes/{index}"),
                    }));
                }

                // find volume mount related to the ingress being cleanup
                if let Some(index) = pod_spec
                    .volumes
                    .as_ref()
                    .and_then(|vols| vols.iter().position(|vol| vol.name == config_name))
                {
                    patches.push(PatchOperation::Remove(RemoveOperation {
                        path: format!("/spec/template/spec/containers/0/volumeMounts/{index}"),
                    }));
                }

                // patch deployment
                deploy_api
                    .patch(
                        "frpc",
                        &PatchParams::default(),
                        &Patch::Json::<()>(json_patch::Patch(patches)),
                    )
                    .await?;

                // delete config map for the ingress
                configmap_api
                    .delete(&cm_name, &DeleteParams::default())
                    .await?;
            }
        }

        return Ok(Action::requeue(Duration::from_secs(60)));
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
    let pod_api: Api<Pod> = Api::all(client.clone());

    let (reader, writer) = reflector::store();
    let stream = reflector(writer, watcher(svc_api, cfg))
        .default_backoff()
        .touched_objects();

    Controller::for_stream(stream, reader)
        .watches(pod_api, watcher::Config::default(), |pod| {
            Some(reflector::ObjectRef::new(pod.name_any().as_str()))
        })
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
