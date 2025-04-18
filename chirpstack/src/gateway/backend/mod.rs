use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::RwLock;

#[cfg(test)]
pub mod mock;

lazy_static! {
    static ref BACKENDS: RwLock<HashMap<String, Box<dyn GatewayBackend + Sync + Send>>> =
        RwLock::new(HashMap::new());
}

#[async_trait]
pub trait GatewayBackend {
    async fn send_downlink(&self, df: &chirpstack_api::gw::DownlinkFrame) -> Result<()>;
    async fn send_configuration(
        &self,
        gw_conf: &chirpstack_api::gw::GatewayConfiguration,
    ) -> Result<()>;
}

pub async fn set_backend(region_config_id: &str, b: Box<dyn GatewayBackend + Sync + Send>) {
    let mut b_w = BACKENDS.write().await;
    b_w.insert(region_config_id.to_string(), b);
}

pub async fn send_downlink(
    region_config_id: &str,
    df: &chirpstack_api::gw::DownlinkFrame,
) -> Result<()> {
    let b_r = BACKENDS.read().await;
    let b = b_r.get(region_config_id).ok_or_else(|| {
        anyhow!(
            "region_config_id '{}' does not exist in BACKENDS",
            region_config_id
        )
    })?;

    b.send_downlink(df).await?;

    Ok(())
}

pub async fn send_configuration(
    region_config_id: &str,
    gw_conf: &chirpstack_api::gw::GatewayConfiguration,
) -> Result<()> {
    let b_r = BACKENDS.read().await;
    let b = b_r.get(region_config_id).ok_or_else(|| {
        anyhow!(
            "region_config_id '{}' does not exist in BACKENDS",
            region_config_id
        )
    })?;

    b.send_configuration(gw_conf).await?;

    Ok(())
}
