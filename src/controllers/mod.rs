use std::{collections::BTreeMap, sync::Arc};

use crate::{context::Context, error::Error, OPERATOR_MANAGER};

use json_patch::{PatchOperation, ReplaceOperation};
use k8s_openapi::{
    api::{
        apps::v1::Deployment,
        core::v1::{ConfigMap, ConfigMapVolumeSource, SecretVolumeSource, Volume, VolumeMount},
    },
    apimachinery::pkg::apis::meta::v1::OwnerReference,
};
use kube::{
    api::{ObjectMeta, Patch, PatchParams},
    Api,
};
use log::debug;

use crate::config::ProxyConfig;
use anyhow::anyhow;

pub mod client;
pub mod ingress;
pub mod service;

pub fn configmap_from_proxy(
    oref: &OwnerReference,
    proxy: &ProxyConfig,
    ns: &str,
) -> Result<ConfigMap, Error> {
    let name = format!("config-proxy-{}", proxy.name);
    let filename = format!("proxy-{}.toml", proxy.name);
    let proxy_config = toml::to_string_pretty(&proxy)
        .map_err(|err| anyhow!("failed to serialize proxy config: {err}"))?;

    debug!("config:\n{}", proxy_config);

    let mut data = BTreeMap::new();
    data.insert(filename.to_owned(), proxy_config);

    Ok(ConfigMap {
        metadata: ObjectMeta {
            name: Some(format!("frpc-{name}")),
            namespace: Some(ns.to_owned()),
            owner_references: Some(vec![oref.clone()]),
            ..ObjectMeta::default()
        },
        data: Some(data),
        ..ConfigMap::default()
    })
}

pub fn volumes_from_proxy(
    proxy: &ProxyConfig,
    volumes: &mut Vec<Volume>,
    volume_mounts: &mut Vec<VolumeMount>,
) {
    let name = format!("config-proxy-{}", proxy.name);
    let filename = format!("proxy-{}.toml", proxy.name);

    if volumes.iter().find(|v| v.name == name).is_none() {
        volumes.push(Volume {
            name: name.to_owned(),
            config_map: Some(ConfigMapVolumeSource {
                name: Some(format!("frpc-{name}")),
                ..ConfigMapVolumeSource::default()
            }),
            ..Volume::default()
        });
    }

    if volume_mounts.iter().find(|v| v.name == name).is_none() {
        volume_mounts.push(VolumeMount {
            name: name.clone(),
            mount_path: format!("/etc/frp/{filename}"),
            sub_path: Some(filename.to_owned()),
            read_only: Some(true),
            ..VolumeMount::default()
        });
    }

    for (proxy_name, plugin) in proxy
        .proxies
        .iter()
        .filter_map(|p| p.plugin.as_ref().map(|plugin| (p.name.as_str(), plugin)))
    {
        let Some(secret_name) = plugin.secret_name.as_ref() else {
            continue;
        };

        let cert_name = format!("certs-{proxy_name}");

        if volumes.iter().find(|v| v.name == cert_name).is_some() {
            continue;
        }

        volumes.push(Volume {
            name: cert_name.to_owned(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret_name.clone()),
                ..SecretVolumeSource::default()
            }),
            ..Volume::default()
        });

        if volume_mounts.iter().find(|v| v.name == cert_name).is_some() {
            continue;
        }

        volume_mounts.push(VolumeMount {
            name: cert_name.to_owned(),
            mount_path: format!("/etc/frp/certs/{secret_name}"),
            read_only: Some(true),
            ..VolumeMount::default()
        });
    }
}

pub async fn patch_deployment(
    deploy_api: &Api<Deployment>,
    volumes: &[Volume],
    volume_mounts: &[VolumeMount],
) -> Result<(), Error> {
    let patch = json_patch::Patch(vec![
        PatchOperation::Replace(ReplaceOperation {
            path: "/spec/template/spec/volumes".to_string(),
            value: serde_json::to_value(volumes).map_err(|err| anyhow!("{err}"))?,
        }),
        PatchOperation::Replace(ReplaceOperation {
            path: "/spec/template/spec/containers/0/volumeMounts".to_string(),
            value: serde_json::to_value(volume_mounts).map_err(|err| anyhow!("{err}"))?,
        }),
    ]);

    deploy_api
        .patch(
            "frpc",
            &PatchParams::apply(OPERATOR_MANAGER),
            &Patch::Json::<()>(patch),
        )
        .await?;

    Ok(())
}

pub async fn run() -> Result<(), Error> {
    let client = kube::Client::try_default().await?;

    let ctx = Arc::new(Context { client });

    let client_fut = client::run(ctx.clone());

    let ingress_fut = ingress::run(ctx.clone());

    let service_fut = service::run(ctx.clone());

    let _ = futures_util::join!(client_fut, ingress_fut, service_fut);

    Ok(())
}
