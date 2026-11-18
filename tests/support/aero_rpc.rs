use anyhow::{Context, Result, ensure};
use reqwest::Url;

fn configured(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|url| !url.trim().is_empty())
}

pub fn eth_url() -> Result<String> {
    configured("E2E_RPC_URL").context("Ethereum archive RPC is required")
}

pub fn derived_url(explicit: &str, network: &str) -> Result<String> {
    if let Some(url) = configured(explicit) {
        return Ok(url);
    }
    let mut url = Url::parse(&eth_url()?).context("invalid Ethereum archive URL")?;
    ensure!(
        url.host_str() == Some("eth-mainnet.g.alchemy.com"),
        "{explicit} is required for this provider"
    );
    url.set_host(Some(&format!("{network}.g.alchemy.com")))?;
    Ok(url.to_string())
}
