use std::process::Stdio;

use anyhow::anyhow;
use log::info;
use tokio::{fs, process::Command};

use crate::error::Error;

use self::config::{ClientConfig, ProxyConfig};

pub mod config;

const BASE_CONFIG_DIR: &'static str = "/etc/frp";
const ROOT_CONFIG_PATH: &'static str = "/etc/frp/frpc.toml";

pub async fn read_config_from_file() -> Result<ClientConfig, Error> {
    let contents = fs::read_to_string(ROOT_CONFIG_PATH)
        .await
        .map_err(|err| anyhow!("failed to read config {ROOT_CONFIG_PATH}: {err}"))?;

    let config =
        toml::from_str(&contents).map_err(|err| anyhow!("failed to deserialize config: {err}"))?;

    Ok(config)
}

pub async fn write_config_to_file(config: ClientConfig) -> Result<(), Error> {
    fs::create_dir_all(BASE_CONFIG_DIR)
        .await
        .map_err(|err| anyhow!("failed to create config directory {BASE_CONFIG_DIR}: {err}"))?;

    let contents =
        toml::to_string(&config).map_err(|err| anyhow!("failed to serialize config: {err}"))?;

    fs::write(ROOT_CONFIG_PATH, &contents)
        .await
        .map_err(|err| anyhow!("failed to write config {ROOT_CONFIG_PATH}: {err}"))?;

    info!("wrote root config to {ROOT_CONFIG_PATH}");
    info!("{contents}");

    Ok(())
}

pub async fn read_config_proxy_from_file(name: &str) -> Result<ProxyConfig, Error> {
    let path = format!("{BASE_CONFIG_DIR}/proxy-{name}.toml");
    let contents = fs::read_to_string(&path)
        .await
        .map_err(|err| anyhow!("failed to read config proxy {path}: {err}"))?;

    let config =
        toml::from_str(&contents).map_err(|err| anyhow!("failed to deserialize config: {err}"))?;

    Ok(config)
}

pub async fn write_config_proxy_to_file(config: ProxyConfig) -> Result<(), Error> {
    let contents =
        toml::to_string(&config).map_err(|err| anyhow!("failed to serialize config: {err}"))?;

    let path = format!("{BASE_CONFIG_DIR}/proxy-{}.toml", config.name);
    fs::write(&path, &contents)
        .await
        .map_err(|err| anyhow!("failed to write config proxy {path}: {err}"))?;

    info!("wrote config: {} to {path}", config.name);
    info!("{contents}");

    Ok(())
}

pub async fn remove_config_proxy_file(name: &str) -> Result<(), Error> {
    let path = format!("{BASE_CONFIG_DIR}/proxy-{name}.toml");
    fs::remove_file(&path)
        .await
        .map_err(|err| anyhow!("failed to remove config proxy {path}: {err}"))?;

    Ok(())
}

pub async fn run(config: ClientConfig) -> Result<(), Error> {
    write_config_to_file(config).await?;

    let status = Command::new("/app/frpc")
        .stdin(Stdio::null())
        .args(&["-c", ROOT_CONFIG_PATH])
        .spawn()
        .map_err(|err| anyhow!("failed to spawn frpc: {err}"))?
        .wait()
        .await
        .map_err(|err| anyhow!("frpc output error: {err}"))?;

    if !status.success() {
        return Err(anyhow!("frpc exit with status: {status:?}").into());
    }

    Ok(())
}

pub async fn reload() -> Result<(), Error> {
    let status = Command::new("/app/frpc")
        .stdin(Stdio::null())
        .args(&["reload", "-c", ROOT_CONFIG_PATH])
        .spawn()
        .map_err(|err| anyhow!("failed to spawn frpc: {err}"))?
        .wait()
        .await
        .map_err(|err| anyhow!("frpc output error: {err}"))?;

    if !status.success() {
        return Err(anyhow!("frpc reload exit with status: {status:?}").into());
    }

    Ok(())
}
