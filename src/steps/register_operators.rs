use alloy::{providers::ProviderBuilder, signers::local::PrivateKeySigner};
use eyre::Result;

use crate::commands::deploy::DeployContext;
use crate::config::ChainContract;
use crate::evm::{Operators, broadcast_and_log};
use crate::ui;
use crate::utils::read_contract_address;

pub async fn run(ctx: &DeployContext, private_key: &str) -> Result<()> {
    let signer: PrivateKeySigner = private_key.parse()?;
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(ctx.rpc_url.parse()?);

    let operators_addr =
        read_contract_address(&ctx.target_json, &ctx.axelar_id, ChainContract::Operators).await?;
    let operators = Operators::new(operators_addr, &provider);

    let operator_addrs = ctx.state.env.axelar_operators();

    for op in operator_addrs {
        let already = operators.isOperator(*op).call().await?;
        if already {
            ui::info(&format!("operator {op} already registered, skipping"));
            continue;
        }
        ui::info(&format!("adding operator: {op}"));
        let pending = operators.addOperator(*op).send().await?;
        broadcast_and_log(pending, "tx").await?;
    }

    Ok(())
}
