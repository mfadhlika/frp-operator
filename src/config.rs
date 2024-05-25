use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Default, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoadBalancer {
    pub group: String,
    pub group_key: String,
}

#[derive(Default, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Proxy {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub local_ip: Option<String>,
    pub local_port: Option<u16>,
    pub remote_port: Option<u16>,
    pub custom_domains: Option<Vec<String>>,
    pub locations: Option<Vec<String>>,
    pub plugin: Option<ProxyPlugin>,
    pub load_balancer: Option<LoadBalancer>,
}

#[derive(Default, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Auth {
    pub method: String,
    pub token: Option<String>,
}

#[derive(Default, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebServer {
    pub addr: Option<String>,
    pub port: u16,
}

#[derive(Default, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientConfig {
    pub server_addr: String,
    pub server_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    pub webserver: Option<WebServer>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub proxies: Vec<Proxy>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub includes: Vec<String>,
}

#[derive(Default, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    #[serde(skip)]
    pub name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub proxies: Vec<Proxy>,
}

#[derive(Default, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxyPlugin {
    #[serde(rename = "type")]
    pub type_: String,
    pub local_addr: Option<String>,
    pub crt_path: Option<String>,
    pub key_path: Option<String>,
    pub host_header_rewrite: Option<String>,
    #[serde(skip)]
    pub secret_name: Option<String>,
}
