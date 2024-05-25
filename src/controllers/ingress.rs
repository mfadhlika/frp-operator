use std::{collections::HashMap, sync::Arc, time::Duration};

use futures_util::StreamExt;
use json_patch::{PatchOperation, RemoveOperation};
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{ConfigMap, Secret, Service},
    networking::v1::{
        Ingress, IngressLoadBalancerIngress, IngressLoadBalancerStatus, IngressPortStatus,
        IngressStatus,
    },
};
use kube::{
    api::{DeleteParams, ListParams, ObjectMeta, Patch, PatchParams},
    runtime::{controller::Action, finalizer, reflector, watcher, Controller, WatchStreamExt},
    Api, ResourceExt,
};
use log::{error, info, warn};

use crate::{
    config::{Proxy, ProxyConfig, ProxyPlugin},
    context::Context,
    error::Error,
    OPERATOR_MANAGER,
};
use anyhow::anyhow;

use super::{client::Client, configmap_from_proxy, patch_deployment, volumes_from_proxy};

pub const INGRESS_FINALIZER: &str = "frp-operator.io/ingress-finalizer";

pub async fn proxy_from_ingress(
    ing: &Ingress,
    client: &kube::Client,
    secrets: &mut Vec<Secret>,
) -> Result<ProxyConfig, Error> {
    let mut proxy_config = ProxyConfig {
        name: ing.name_any(),
        ..ProxyConfig::default()
    };

    let ns = ing
        .metadata
        .namespace
        .clone()
        .unwrap_or("default".to_string());
    let svc_api: Api<Service> = Api::namespaced(client.clone(), &ns);
    let secret_api: Api<Secret> = Api::namespaced(client.clone(), &ns);

    let rules = ing.spec.as_ref().unwrap().rules.as_ref().unwrap();
    for rule in rules {
        let custom_domains = rule.host.as_ref().map(|h| vec![h.to_owned()]);
        let paths = &rule.http.as_ref().unwrap().paths;
        for path in paths {
            let backend_svc = path.backend.service.as_ref().unwrap();
            let backend_svc_port = backend_svc.port.as_ref().unwrap();
            let svc_name = &backend_svc.name;
            let svc = svc_api
                .get(&svc_name)
                .await
                .map_err(|err| anyhow!("failed to get service {svc_name}: {err}"))?;
            let svc_spec = svc.spec.as_ref().unwrap();
            let port_name = backend_svc_port.name.as_ref();
            let port_number = backend_svc_port.number.as_ref();
            let hostname = format!("{svc_name}.{ns}.svc.cluster.local");

            let port = if let Some(_) = port_name {
                svc_spec
                    .ports
                    .as_ref()
                    .unwrap()
                    .iter()
                    .find(|port| port.name.as_ref() == port_name)
                    .unwrap()
                    .port as u16
            } else if let Some(port) = port_number {
                *port as u16
            } else {
                panic!("error find port");
            };

            let locations = path.path.as_ref().map(|p| vec![p.to_owned()]);

            proxy_config.proxies.push(Proxy {
                name: ing.name_any(),
                type_: "http".to_string(),
                local_ip: Some(hostname),
                local_port: Some(port),
                custom_domains: custom_domains.to_owned(),
                locations,
                ..Proxy::default()
            })
        }
    }

    let mut tls_map = HashMap::new();
    for ing in ing.spec.as_ref().unwrap().tls.iter().flatten() {
        for host in ing.hosts.as_ref().unwrap() {
            tls_map.insert(host.to_string(), ing.secret_name.clone().unwrap());
        }
    }

    for proxy in proxy_config.proxies.iter_mut() {
        for domain in proxy.custom_domains.as_ref().unwrap() {
            if let Some(secret_name) = tls_map.get(domain) {
                let Ok(secret) = secret_api.get(secret_name).await else {
                    continue;
                };

                secrets.push(secret);

                proxy.type_ = "https".to_string();
                proxy.plugin = Some(ProxyPlugin {
                    type_: "https2http".to_string(),
                    local_addr: proxy
                        .local_ip
                        .as_ref()
                        .zip(proxy.local_port)
                        .map(|(ip, port)| format!("{ip}:{port}")),
                    crt_path: Some(format!("/etc/frp/certs/{secret_name}/tls.crt")),
                    key_path: Some(format!("/etc/frp/certs/{secret_name}/tls.key")),
                    secret_name: Some(secret_name.to_owned()),
                    ..ProxyPlugin::default()
                });

                proxy.locations = None;
                proxy.local_ip = None;
                proxy.local_port = None;
            }
        }
    }

    Ok(proxy_config)
}

async fn reconcile(obj: Arc<Ingress>, ctx: Arc<Context>) -> Result<Action, Error> {
    if !obj
        .metadata
        .annotations
        .as_ref()
        .and_then(|ann| ann.get("kubernetes.io/ingress.class"))
        .or(obj
            .spec
            .as_ref()
            .and_then(|spec| spec.ingress_class_name.as_ref()))
        .map_or(false, |ic| ic == "frp")
    {
        return Ok(Action::requeue(Duration::from_secs(60)));
    }

    let obj_name = obj.name_any().to_owned();
    let obj_spec = obj.spec.to_owned();

    let client = ctx.client.clone();

    let cli_api: Api<Client> = Api::all(client.clone());

    let clis = cli_api.list(&ListParams::default().timeout(30)).await?;
    let cli = clis
        .iter()
        .nth(0)
        .ok_or_else(|| anyhow!("no client found"))?;

    let cli_ns = cli.namespace().unwrap_or("default".to_string());
    let configmap_api: Api<ConfigMap> = Api::namespaced(client.clone(), &cli_ns);

    let obj_ns = obj.namespace().unwrap_or("default".to_string());

    let name = format!("config-proxy-{}", obj.name_any());
    let cm_name = format!("frpc-{name}");

    let ingress_api: Api<Ingress> = Api::namespaced(client.clone(), &obj_ns);
    let deploy_api: Api<Deployment> = Api::namespaced(client.clone(), &cli_ns);
    let secret_api: Api<Secret> = Api::namespaced(client.clone(), &cli_ns);

    let dep: Deployment = deploy_api.get("frpc").await?;

    finalizer(&ingress_api, INGRESS_FINALIZER, obj, |event| async {
        match event {
            finalizer::Event::Apply(ing) => {
                let oref = dep.owner_references().get(0).unwrap();

                let mut secrets = vec![];
                let proxy = proxy_from_ingress(&ing, &client, &mut secrets).await?;

                for secret in secrets {
                    let secret = Secret {
                        metadata: ObjectMeta {
                            name: secret.metadata.name.clone(),
                            namespace: Some(cli_ns.clone()),
                            owner_references: Some(vec![oref.clone()]),
                            ..ObjectMeta::default()
                        },
                        data: secret.data.clone(),
                        ..Secret::default()
                    };
                    secret_api
                        .patch(
                            secret.name_any().as_str(),
                            &PatchParams::apply(OPERATOR_MANAGER),
                            &Patch::Apply(secret.to_owned()),
                        )
                        .await?;
                }

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

                let mut ing = ingress_api.get_status(&obj_name).await?;
                ing.status = Some(IngressStatus {
                    load_balancer: Some(IngressLoadBalancerStatus {
                        ingress: Some(vec![IngressLoadBalancerIngress {
                            // hostname: todo!(),
                            ip: Some(cli.spec.server_addr.to_owned()),
                            ports: Some(vec![IngressPortStatus {
                                port: 80,
                                protocol: "TCP".to_string(),
                                ..IngressPortStatus::default()
                            }]),
                            ..IngressLoadBalancerIngress::default()
                        }]),
                    }),
                });

                ingress_api
                    .patch_status(
                        &obj_name,
                        &PatchParams::apply(OPERATOR_MANAGER),
                        &Patch::Merge(ing),
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
                    .and_then(|vms| vms.iter().position(|vol| vol.name == name))
                {
                    patches.push(PatchOperation::Remove(RemoveOperation {
                        path: format!("/spec/template/spec/volumes/{index}"),
                    }));
                }

                // find volume mount related to the ingress being cleanup
                if let Some(index) = pod_spec
                    .volumes
                    .as_ref()
                    .and_then(|vols| vols.iter().position(|vol| vol.name == name))
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

                // delete secrets for the ingress
                for secret_name in obj_spec
                    .as_ref()
                    .and_then(|spec| spec.tls.as_ref())
                    .into_iter()
                    .flatten()
                    .filter_map(|tls| tls.secret_name.as_ref())
                {
                    secret_api
                        .delete(secret_name, &DeleteParams::default())
                        .await?;
                }
            }
        }

        Ok(Action::requeue(Duration::from_secs(60)))
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
    let ingress_api: Api<Ingress> = Api::all(client.clone());

    let (reader, writer) = reflector::store();
    let stream = reflector(writer, watcher(ingress_api, cfg))
        .default_backoff()
        .touched_objects();

    Controller::for_stream(stream, reader)
        .shutdown_on_signal()
        .run(reconcile, error_policy, ctx.clone())
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("reconciled ingress {:?}", o),
                Err(e) => warn!("reconcile ingress failed: {:?}", e),
            }
        })
        .await;

    Ok(())
}
