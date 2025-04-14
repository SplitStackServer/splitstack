use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::RwLock;

#[cfg(test)]
pub mod mock;

lazy_static! {
    static ref BACKENDS: RwLock<HashMap<String, Box<dyn BasestationBackend + Sync + Send>>> =
        RwLock::new(HashMap::new());
}

#[async_trait]
pub trait BasestationBackend {
    async fn send_command(&self, cmd: &chirpstack_api::bs::ProtoCommand) -> Result<()>;
    async fn send_response(&self, rsp: &chirpstack_api::bs::ProtoResponse) -> Result<()>;
}

pub async fn set_backend(region_config_id: &str, b: Box<dyn BasestationBackend + Sync + Send>) {
    let mut b_w = BACKENDS.write().await;
    b_w.insert(region_config_id.to_string(), b);
}

pub async fn send_command(
    region_config_id: &str,
    cmd: &chirpstack_api::bs::ProtoCommand,
) -> Result<()> {
    let b_r = BACKENDS.read().await;
    let b = b_r.get(region_config_id).ok_or_else(|| {
        anyhow!(
            "region_config_id '{}' does not exist in BACKENDS",
            region_config_id
        )
    })?;

    b.send_command(cmd).await?;

    Ok(())
}

pub async fn send_response(
    region_config_id: &str,
    rsp: &chirpstack_api::bs::ProtoResponse,
) -> Result<()> {
    let b_r = BACKENDS.read().await;
    let b = b_r.get(region_config_id).ok_or_else(|| {
        anyhow!(
            "region_config_id '{}' does not exist in BACKENDS",
            region_config_id
        )
    })?;

    b.send_response(rsp).await?;

    Ok(())
}
