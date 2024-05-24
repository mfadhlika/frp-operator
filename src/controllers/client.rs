use crate::error::Error;
use crate::OPERATOR_MANAGER;
use crate::{config::*, context::Context};
use anyhow::anyhow;
use futures_util::StreamExt;
use k8s_openapi::{
    api::{
        apps::v1::{Deployment, DeploymentSpec},
        core::v1::{
            ConfigMap, ConfigMapVolumeSource, Container, EnvFromSource, PodSpec, PodTemplateSpec,
            SecretEnvSource, Volume, VolumeMount,
        },
    },
    apimachinery::pkg::apis::meta::v1::LabelSelector,
    Metadata,
};
use kube::{
    api::{ObjectMeta, Patch, PatchParams},
    runtime::{controller::Action, watcher, Controller},
    Api, CustomResource, Resource,
};
use log::{error, info, warn};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt::Debug, sync::Arc, time::Duration};

#[derive(Default, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuthSpec {
    secret: Option<String>,
    token: Option<String>,
}

#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[kube(group = "frp-operator.io", version = "v1", kind = "Client", namespaced)]
#[serde(rename_all = "camelCase")]
pub struct ClientSpec {
    pub server_addr: String,
    pub server_port: u16,
    pub webserver_addr: Option<String>,
    pub webserver_port: Option<u16>,
    pub auth: Option<AuthSpec>,
}

impl Client {
    async fn reconcile(&self, ctx: Arc<Context>) -> Result<Action, Error> {
        let client = ctx.client.clone();

        let ns = self
            .metadata
            .namespace
            .clone()
            .unwrap_or("default".to_string());

        let cm_api: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);
        let dep_api: Api<Deployment> = Api::namespaced(client.clone(), &ns);

        let mut labels = BTreeMap::new();
        labels.insert(
            "app.kubernetes.io/part-of".to_string(),
            "frp-operator".to_string(),
        );
        labels.insert("app.kubernetes.io/name".to_string(), "frpc".to_string());

        let config = ClientConfig {
            server_addr: self.spec.server_addr.clone(),
            server_port: self.spec.server_port,
            auth: self.spec.auth.is_some().then(|| Auth {
                method: "token".to_string(),
                token: Some("{{ .Envs.FRP_AUTH_TOKEN }}".to_string()),
            }),
            webserver: self.spec.webserver_port.map(|port| WebServer {
                addr: self.spec.webserver_addr.to_owned(),
                port,
            }),
            includes: vec!["/etc/frp/proxy-*.toml".to_string()],
            ..ClientConfig::default()
        };

        let env_from = self
            .spec
            .auth
            .as_ref()
            .and_then(|auth| auth.secret.to_owned())
            .map(|secret| {
                vec![EnvFromSource {
                    secret_ref: Some(SecretEnvSource {
                        name: Some(secret),
                        ..SecretEnvSource::default()
                    }),
                    ..EnvFromSource::default()
                }]
            });

        let cm_data = {
            let client_config =
                toml::to_string_pretty(&config).map_err(|err| anyhow!("{}", err))?;
            info!("config:\n{}", client_config);

            let mut data = BTreeMap::new();
            data.insert("frpc.toml".to_string(), client_config);
            Some(data)
        };

        let oref = self.controller_owner_ref(&()).unwrap();

        let cms = vec![ConfigMap {
            metadata: ObjectMeta {
                name: Some("frpc-config".to_string()),
                namespace: Some(ns.to_owned()),
                owner_references: Some(vec![oref.clone()]),
                ..ObjectMeta::default()
            },
            data: cm_data,
            ..ConfigMap::default()
        }];

        let volumes = vec![Volume {
            name: "frpc-config".to_string(),
            config_map: Some(ConfigMapVolumeSource {
                name: Some("frpc-config".to_string()),
                ..ConfigMapVolumeSource::default()
            }),
            ..Volume::default()
        }];

        let volume_mounts = vec![VolumeMount {
            name: "frpc-config".to_string(),
            mount_path: "/etc/frp/frpc.toml".to_string(),
            sub_path: Some("frpc.toml".to_string()),
            read_only: Some(true),
            ..VolumeMount::default()
        }];

        let deployment = Deployment {
            metadata: ObjectMeta {
                name: Some(format!("frpc")),
                namespace: Some(ns.to_owned()),
                labels: Some(labels.clone()),
                owner_references: Some(vec![oref.clone()]),
                ..ObjectMeta::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(1),
                selector: LabelSelector {
                    match_labels: Some(labels.clone()),
                    ..LabelSelector::default()
                },
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        name: Some(format!("frpc")),
                        labels: Some(labels.clone()),
                        ..ObjectMeta::default()
                    }),
                    spec: Some(PodSpec {
                        volumes: Some(volumes),
                        containers: vec![Container {
                            name: "frpc".to_string(),
                            image: Some("docker.io/snowdreamtech/frpc:latest".to_string()),
                            volume_mounts: Some(volume_mounts),
                            env_from: env_from,
                            ..Container::default()
                        }],
                        ..PodSpec::default()
                    }),
                    ..PodTemplateSpec::default()
                },
                ..DeploymentSpec::default()
            }),
            ..Deployment::default()
        };

        for cm in cms {
            cm_api
                .patch(
                    cm.metadata()
                        .name
                        .as_ref()
                        .ok_or_else(|| anyhow!("configmap missing name"))?,
                    &PatchParams::apply(OPERATOR_MANAGER),
                    &Patch::Apply(&cm),
                )
                .await?;
        }

        dep_api
            .patch(
                deployment
                    .metadata()
                    .name
                    .as_ref()
                    .ok_or_else(|| anyhow!("deployment missing name"))?,
                &PatchParams::apply(OPERATOR_MANAGER),
                &Patch::Apply(&deployment),
            )
            .await?;

        Ok(Action::requeue(Duration::from_secs(60)))
    }
}

async fn reconcile(obj: Arc<Client>, ctx: Arc<Context>) -> Result<Action, Error> {
    return obj.reconcile(ctx).await;
}

fn error_policy<K>(_obj: Arc<K>, err: &Error, _ctx: Arc<Context>) -> Action {
    error!("reason: {}", err);

    Action::requeue(Duration::from_secs(15))
}

pub async fn run(ctx: Arc<Context>) -> anyhow::Result<()> {
    let client = ctx.client.clone();

    let client_api: Api<Client> = Api::all(client.clone());

    Controller::new(client_api, watcher::Config::default())
        .shutdown_on_signal()
        .run(reconcile, error_policy, ctx.clone())
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("reconciled client {:?}", o),
                Err(e) => warn!("reconcile client failed: {:?}", e),
            }
        })
        .await;

    Ok(())
}
