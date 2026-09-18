//! Bind the signing identity to the account queried by the strategy.
use super::Client;
use alloy::primitives::Address;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

fn address(v: &Value) -> Result<Address> {
    v.as_str()
        .context("authorization address missing")?
        .parse()
        .context("invalid authorization address")
}
pub async fn verify(client: &Client, signer: Address) -> Result<Value> {
    let master: Address = client
        .cfg
        .account
        .as_deref()
        .context("configured master account required")?
        .parse()?;
    let target: Address = client.user()?.parse()?;
    ensure!(
        client.cfg.vault.is_none() || target != master,
        "vault/subaccount address must differ from master account"
    );
    let role = client
        .info(json!({"type":"userRole","user":master}))
        .await?;
    ensure!(
        role["role"] == "user",
        "configured account is not a Hyperliquid master user"
    );
    if target != master {
        let role = client
            .info(json!({"type":"userRole","user":target}))
            .await?;
        match role["role"].as_str() {
            Some("subAccount") => ensure!(
                address(&role["data"]["master"])? == master,
                "subaccount belongs to another master"
            ),
            Some("vault") => {
                let vault = client
                    .info(json!({"type":"vaultDetails","vaultAddress":target}))
                    .await?;
                ensure!(
                    address(&vault["vaultAddress"])? == target,
                    "vault response identity mismatch"
                );
                ensure!(
                    address(&vault["leader"])? == master,
                    "vault leader differs from configured master"
                );
            }
            _ => anyhow::bail!("target is not a verified subaccount/vault"),
        }
    }
    let mut expires = None;
    if signer != master {
        let role = client
            .info(json!({"type":"userRole","user":signer}))
            .await?;
        ensure!(
            role["role"] == "agent" && address(&role["data"]["user"])? == master,
            "API signer is not an agent of the configured master"
        );
        let agents = client
            .info(json!({"type":"extraAgents","user":master}))
            .await?;
        let grant = agents
            .as_array()
            .context("invalid agent grants")?
            .iter()
            .find(|v| address(&v["address"]).is_ok_and(|a| a == signer))
            .context("agent has no verifiable grant; use an authorized named API wallet")?;
        let until = grant["validUntil"]
            .as_u64()
            .context("agent expiration missing")?;
        ensure!(
            until > crate::now_ms().saturating_add(60_000),
            "API agent expired or expires within one minute"
        );
        expires = Some(until);
    }
    Ok(
        json!({"mainnet":client.cfg.mainnet,"master":master,"user":target,"signer":signer,
        "agent_valid_until":expires,"verified_ms":crate::now_ms()}),
    )
}
