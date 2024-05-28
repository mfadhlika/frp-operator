use std::{collections::HashMap, sync::Arc, time::Duration};

use futures_util::StreamExt;
use k8s_openapi::api::{
    core::v1::{Secret, Service},
    networking::v1::{
        Ingress, IngressLoadBalancerIngress, IngressLoadBalancerStatus, IngressPortStatus,
        IngressStatus,
    },
};
use kube::{
    api::{Patch, PatchParams},
    runtime::{controller::Action, finalizer, reflector, watcher, Controller, WatchStreamExt},
    Api, ResourceExt,
};
use log::{error, info, warn};
use tokio::fs;

use crate::{
    context::Context,
    error::Error,
    frpc::{
        self,
        config::{Proxy, ProxyConfig, ProxyPlugin},
    },
    OPERATOR_MANAGER,
};
use anyhow::anyhow;

pub const INGRESS_FINALIZER: &str = "frp-operator.io/ingress-finalizer";

pub async fn proxy_from_ingress(
    ing: &Ingress,
    client: &kube::Client,
    secrets: &mut Vec<Secret>,
) -> Result<ProxyConfig, Error> {
    let mut config = ProxyConfig {
        name: ing.name_any(),
        proxies: vec![],
    };

    let ns: String = ing.namespace().unwrap_or("default".to_string());
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

            let port = if let Some(port) = port_number {
                *port as u16
            } else if let Some(port) = svc_spec
                .ports
                .iter()
                .flatten()
                .find(|port| port.name.as_ref() == port_name)
            {
                port.port as u16
            } else {
                return Err(anyhow!("failed to find port").into());
            };

            let locations = path.path.as_ref().map(|p| vec![p.to_owned()]);

            config.proxies.push(Proxy {
                name: format!("ing-{}", ing.name_any()),
                type_: "http".to_string(),
                local_ip: Some(format!("{svc_name}.{ns}.svc.cluster.local")),
                local_port: Some(port),
                custom_domains: custom_domains.to_owned(),
                locations,
                ..Proxy::default()
            });
        }
    }

    let mut tls_map = HashMap::new();
    for ing in ing.spec.as_ref().unwrap().tls.iter().flatten() {
        for host in ing.hosts.as_ref().unwrap() {
            tls_map.insert(host.to_string(), ing.secret_name.clone().unwrap());
        }
    }

    for proxy in config.proxies.iter_mut() {
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
                    crt_path: Some(format!("/etc/ssl/certs/{secret_name}/tls.crt")),
                    key_path: Some(format!("/etc/ssl/certs/{secret_name}/tls.key")),
                    secret_name: Some(secret_name.to_owned()),
                    ..ProxyPlugin::default()
                });

                proxy.locations = None;
                proxy.local_ip = None;
                proxy.local_port = None;
            }
        }
    }

    Ok(config)
}

async fn reconcile(obj: Arc<Ingress>, ctx: Arc<Context>) -> Result<Action, Error> {
    if !obj
        .annotations()
        .get("kubernetes.io/ingress.class")
        .or(obj
            .spec
            .as_ref()
            .and_then(|spec| spec.ingress_class_name.as_ref()))
        .map_or(false, |ic| ic == "frp")
    {
        return Ok(Action::await_change());
    }

    let obj_name = obj.name_any().to_owned();
    let obj_ns = obj.namespace().unwrap_or("default".to_string());

    let client = ctx.client.clone();
    let ingress_api: Api<Ingress> = Api::namespaced(client.clone(), &obj_ns);

    finalizer(&ingress_api, INGRESS_FINALIZER, obj, |event| async {
        match event {
            finalizer::Event::Apply(ing) => {
                let mut secrets = vec![];
                let config = proxy_from_ingress(&ing, &client, &mut secrets).await?;

                frpc::write_config_proxy_to_file(config).await?;

                for secret in secrets {
                    // copy secret data
                    for (key, contents) in secret.data.iter().flatten() {
                        let dir = format!("/etc/ssl/certs/{}/{}", obj_name, secret.name_any());
                        let path = format!("{dir}/{key}");
                        if fs::try_exists(&path).await? {
                            continue;
                        };
                        fs::create_dir_all(dir).await?;
                        fs::write(&path, &contents.0)
                            .await
                            .map_err(|err| anyhow!("failed to write secret {key}: {err}"))?;
                    }
                }

                frpc::reload().await?;

                let mut ing = ingress_api.get_status(&obj_name).await?;
                ing.status = Some(IngressStatus {
                    load_balancer: Some(IngressLoadBalancerStatus {
                        ingress: Some(vec![IngressLoadBalancerIngress {
                            // hostname: todo!(),
                            ip: frpc::read_config_from_file()
                                .await
                                .map(|config| config.server_addr)
                                .ok(),
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
            finalizer::Event::Cleanup(ing) => {
                frpc::remove_config_proxy_from_file(&ing.name_any()).await?;

                for secret_name in ing
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.tls.clone())
                    .iter()
                    .flatten()
                    .filter_map(|s| s.secret_name.clone())
                {
                    fs::remove_dir_all(format!("/etc/ssl/certs/{secret_name}")).await?;
                }

                frpc::reload().await?;
            }
        }

        Ok(Action::requeue(Duration::from_secs(3600)))
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
