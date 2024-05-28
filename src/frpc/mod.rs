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
        .map_err(|err| anyhow!("failed to read config: {err}"))?;

    let config =
        toml::from_str(&contents).map_err(|err| anyhow!("failed to deserialize config: {err}"))?;

    Ok(config)
}

pub async fn write_config_to_file(config: ClientConfig) -> Result<(), Error> {
    fs::create_dir_all(BASE_CONFIG_DIR)
        .await
        .map_err(|err| anyhow!("failed to create config directory: {err}"))?;

    let contents =
        toml::to_string(&config).map_err(|err| anyhow!("failed to serialize config: {err}"))?;

    fs::write(ROOT_CONFIG_PATH, &contents)
        .await
        .map_err(|err| anyhow!("failed to write config: {err}"))?;

    info!("wrote root config to {ROOT_CONFIG_PATH}");
    info!("{contents}");

    Ok(())
}

pub async fn read_config_proxy_from_file(name: &str) -> Result<ProxyConfig, Error> {
    let path = format!("{BASE_CONFIG_DIR}/proxy-{name}.toml");
    let contents = fs::read_to_string(path)
        .await
        .map_err(|err| anyhow!("failed to read config proxy: {err}"))?;

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
        .map_err(|err| anyhow!("failed to write config  proxy: {err}"))?;

    info!("wrote config: {} to {path}", config.name);
    info!("{contents}");

    Ok(())
}

pub async fn remove_config_proxy_from_file(name: &str) -> Result<(), Error> {
    let path = format!("{BASE_CONFIG_DIR}/proxy-{name}.toml");
    fs::remove_file(&path)
        .await
        .map_err(|err| anyhow!("failed to remove config proxy: {err}"))?;

    Ok(())
}

pub async fn run(config: ClientConfig) -> Result<(), Error> {
    let mut frpc_cmd = Command::new("/app/frpc");

    frpc_cmd.stdout(Stdio::piped());
    frpc_cmd.stderr(Stdio::piped());

    write_config_to_file(config).await?;

    frpc_cmd.args(&["-c", ROOT_CONFIG_PATH]);

    let child = frpc_cmd
        .spawn()
        .map_err(|err| anyhow!("failed to spawn frpc: {err}"))?;

    let output = child
        .wait_with_output()
        .await
        .map_err(|err| anyhow!("frpc output error: {err}"))?;

    if !output.status.success() {
        return Err(anyhow!("frpc output error: {output:?}").into());
    }

    Ok(())
}

pub async fn reload() -> Result<(), Error> {
    let mut frpc_cmd = Command::new("/app/frpc");

    frpc_cmd.stdout(Stdio::piped());
    frpc_cmd.stderr(Stdio::piped());

    frpc_cmd.args(&["reload", "-c", ROOT_CONFIG_PATH]);

    let child = frpc_cmd
        .spawn()
        .map_err(|err| anyhow!("failed to spawn frpc: {err}"))?;

    let output = child
        .wait_with_output()
        .await
        .map_err(|err| anyhow!("frpc output error: {err}"))?;

    if !output.status.success() {
        return Err(anyhow!("frpc output error: {output:?}").into());
    }

    Ok(())
}
