//! Building blocks for `axe propose`: config resolution, calldata + governance
//! payload encoding, and on-chain verification that the target ASG is real and
//! correctly wired. The orchestrator in `mod.rs` reads as a sequence of these.

use std::str::FromStr;

use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use eyre::{Result, WrapErr};

use serde_json::Value;

use crate::config::{AxelarChainContract, AxelarGlobalContract, ChainContract, ChainsConfig};
use crate::cosmos::lcd_query_proposal;
use crate::evm::AxelarServiceGovernance;
use crate::timing::COSMOS_PROPOSAL_POLL_INTERVAL;
use crate::types::Network;
use crate::ui;

use super::types::{Operation, ProposalType, ResolvedConfig, TargetContract};

sol! {
    function setPauseStatus(bool paused);
    function setTrustedChain(string chainName);
    function removeTrustedChain(string chainName);
}

/// What the on-chain ASG reports about itself — surfaced to the user and used
/// to gate `--relay` (the operator key must match `operator`).
#[derive(Clone, Debug)]
pub struct AsgInfo {
    pub operator: Address,
    pub governance_chain: String,
    pub governance_address: String,
    pub minimum_time_lock_delay: u64,
}

/// Pull every address and parameter the proposal needs out of the chain config.
/// Errors early (with a chain-specific message) if anything required is absent.
pub fn resolve(network: Network, config: &ChainsConfig, chain: &str) -> Result<ResolvedConfig> {
    let chain_cfg = config
        .chains
        .get(chain)
        .ok_or_else(|| eyre::eyre!("chain '{chain}' not found in {network} config"))?;
    let edge_axelar_id = chain_cfg.axelar_id_or(chain);

    let asg_address = chain_cfg
        .contract_address(ChainContract::AxelarServiceGovernance, &edge_axelar_id)
        .wrap_err("target chain has no AxelarServiceGovernance — deploy/transfer ownership first")?
        .to_string();
    let gateway_address = chain_cfg
        .contract_address(ChainContract::AxelarGateway, &edge_axelar_id)?
        .to_string();
    let its_address = chain_cfg
        .contract_address(ChainContract::InterchainTokenService, &edge_axelar_id)
        .ok()
        .map(str::to_string);

    let edge_rpc = chain_cfg
        .rpc
        .clone()
        .ok_or_else(|| eyre::eyre!("no rpc for chain '{chain}' in config"))?;
    let multisig_prover = config
        .axelar
        .contract_address(AxelarChainContract::MultisigProver, &edge_axelar_id)?
        .to_string();
    let axelar_rpc = config.axelar.rpc.clone().ok_or_else(|| {
        eyre::eyre!("no axelar.rpc (Tendermint RPC) in config — needed for relay")
    })?;

    let axelarnet_gateway = config
        .axelar
        .global_contract_address(AxelarGlobalContract::AxelarnetGateway)?
        .to_string();
    let gov_module = config
        .axelar
        .governance_address
        .clone()
        .ok_or_else(|| eyre::eyre!("no axelar.governanceAddress (gov module) in config"))?;
    let (lcd, chain_id, fee_denom, gas_price) = config.axelar.cosmos_tx_params()?;
    let deposit_amount = config
        .axelar
        .gov_proposal_deposit_amount
        .clone()
        .unwrap_or_else(|| DEFAULT_DEPOSIT.to_string());
    let expedited_deposit_amount = config
        .axelar
        .gov_proposal_expedited_deposit_amount
        .clone()
        .unwrap_or_else(|| DEFAULT_EXPEDITED_DEPOSIT.to_string());

    Ok(ResolvedConfig {
        edge_axelar_id,
        asg_address,
        gateway_address,
        its_address,
        edge_rpc,
        multisig_prover,
        axelar_rpc,
        axelarnet_gateway,
        gov_module,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        deposit_amount,
        expedited_deposit_amount,
    })
}

/// Friendly label for a target address if it's a known contract in this chain's
/// config (e.g. `"hedera ITS"`); `None` if the address isn't a known
/// contract — the caller renders that as a red "Unknown Destination".
pub fn target_name(config: &ChainsConfig, chain: &str, target: Address) -> Option<String> {
    let chain_cfg = config.chains.get(chain)?;
    let contracts = chain_cfg.contracts.as_ref()?;
    let id = chain_cfg.axelar_id_or(chain);
    contracts.iter().find_map(|(name, entry)| {
        let addr = entry.address.as_deref()?;
        (Address::from_str(addr).ok() == Some(target))
            .then(|| format!("{id} {}", friendly_contract(name)))
    })
}

/// Short, human label for the common governance-target contracts.
fn friendly_contract(contract: &str) -> &str {
    match contract {
        "AxelarGateway" => "gateway",
        "InterchainTokenService" => "ITS",
        "AxelarServiceGovernance" => "ASG",
        "InterchainTokenFactory" => "ITS Factory",
        other => other,
    }
}

/// Resolve `(target, calldata)` for a catalog operation against the config.
pub fn build_call(
    op: Operation,
    cfg: &ResolvedConfig,
    its_chain: Option<&str>,
) -> Result<(Address, Bytes)> {
    let target = target_address(op.target_contract(), cfg)?;
    let calldata = match op {
        Operation::Pause | Operation::ItsPause => encode_set_pause(true),
        Operation::Unpause => encode_set_pause(false),
        Operation::SetTrusted => encode_set_trusted(require_its_chain(op, its_chain)?),
        Operation::RemoveTrusted => encode_remove_trusted(require_its_chain(op, its_chain)?),
    };
    Ok((target, calldata))
}

fn target_address(target: TargetContract, cfg: &ResolvedConfig) -> Result<Address> {
    let raw = match target {
        TargetContract::Gateway => &cfg.gateway_address,
        TargetContract::Its => cfg.its_address.as_ref().ok_or_else(|| {
            eyre::eyre!("operation targets ITS but no InterchainTokenService in config")
        })?,
    };
    parse_address(raw, target.config_key())
}

fn require_its_chain(op: Operation, its_chain: Option<&str>) -> Result<&str> {
    its_chain.ok_or_else(|| eyre::eyre!("operation '{}' requires --its-chain", op.label()))
}

fn encode_set_pause(paused: bool) -> Bytes {
    setPauseStatusCall { paused }.abi_encode().into()
}

fn encode_set_trusted(chain_name: &str) -> Bytes {
    setTrustedChainCall {
        chainName: chain_name.to_string(),
    }
    .abi_encode()
    .into()
}

fn encode_remove_trusted(chain_name: &str) -> Bytes {
    removeTrustedChainCall {
        chainName: chain_name.to_string(),
    }
    .abi_encode()
    .into()
}

/// `abi.encode(uint256 command, address target, bytes callData, uint256 nativeValue, uint256 eta)`
/// — the AxelarServiceGovernance proposal payload. `callData` at offset 0xa0 is
/// the canonical governance discriminator (vs the ITS msgType-0 collision).
pub fn encode_governance_payload(command: u8, target: Address, calldata: Bytes, eta: u64) -> Bytes {
    (
        U256::from(command),
        target,
        calldata,
        U256::ZERO,
        U256::from(eta),
    )
        .abi_encode_params()
        .into()
}

/// Read the ASG's own view of itself and assert it's a real, correctly-wired
/// governance contract: code present, and `governanceAddress` == the gov module
/// we'll submit from (else its `onlyGovernance` guard would reject the message).
pub async fn verify_asg(cfg: &ResolvedConfig) -> Result<AsgInfo> {
    let address = parse_address(&cfg.asg_address, "AxelarServiceGovernance")?;
    let provider = ProviderBuilder::new().connect_http(
        cfg.edge_rpc
            .parse()
            .wrap_err_with(|| format!("invalid rpc url '{}'", cfg.edge_rpc))?,
    );

    let code = provider
        .get_code_at(address)
        .await
        .wrap_err("failed to read ASG code")?;
    if code.is_empty() {
        return Err(eyre::eyre!(
            "no contract code at ASG address {} — not a deployed ASG",
            cfg.asg_address
        ));
    }

    let asg = AxelarServiceGovernance::new(address, &provider);
    let operator = asg
        .operator()
        .call()
        .await
        .wrap_err("ASG.operator() failed — not an ASG?")?;
    let governance_chain = asg
        .governanceChain()
        .call()
        .await
        .wrap_err("ASG.governanceChain() failed")?;
    let governance_address = asg
        .governanceAddress()
        .call()
        .await
        .wrap_err("ASG.governanceAddress() failed")?;
    let delay = asg
        .minimumTimeLockDelay()
        .call()
        .await
        .wrap_err("ASG.minimumTimeLockDelay() failed")?;

    if !governance_address.eq_ignore_ascii_case(&cfg.gov_module) {
        return Err(eyre::eyre!(
            "ASG.governanceAddress() = {governance_address} does not match the gov module {} — \
             a proposal from gov would be rejected by onlyGovernance",
            cfg.gov_module
        ));
    }

    Ok(AsgInfo {
        operator,
        governance_chain,
        governance_address,
        minimum_time_lock_delay: u256_to_u64(delay),
    })
}

/// Idempotency guard: refuse to re-submit a proposal the ASG already holds
/// (an approved operator proposal, or a scheduled time-lock with a non-zero eta).
pub async fn check_not_already_proposed(
    cfg: &ResolvedConfig,
    ptype: ProposalType,
    target: Address,
    calldata: Bytes,
) -> Result<()> {
    let address = parse_address(&cfg.asg_address, "AxelarServiceGovernance")?;
    let provider = ProviderBuilder::new().connect_http(
        cfg.edge_rpc
            .parse()
            .wrap_err_with(|| format!("invalid rpc url '{}'", cfg.edge_rpc))?,
    );
    let asg = AxelarServiceGovernance::new(address, &provider);

    match ptype {
        ProposalType::Operator => {
            let approved = asg
                .isOperatorProposalApproved(target, calldata, U256::ZERO)
                .call()
                .await
                .wrap_err("ASG.isOperatorProposalApproved() failed")?;
            if approved {
                return Err(eyre::eyre!(
                    "this operator proposal is already approved on the ASG — nothing to submit"
                ));
            }
        }
        ProposalType::Timelock => {
            let eta = asg
                .getProposalEta(target, calldata, U256::ZERO)
                .call()
                .await
                .wrap_err("ASG.getProposalEta() failed")?;
            if eta != U256::ZERO {
                return Err(eyre::eyre!(
                    "this time-lock proposal is already scheduled (eta={eta}) — nothing to submit"
                ));
            }
        }
    }
    Ok(())
}

/// Poll the proposal on the hub until it reaches a terminal status. `Ok(())`
/// means it passed; rejection/failure (including on-chain execution failure)
/// returns an error with the tally. Mirrors the deployer's `cosmos-poll` step.
pub async fn monitor_proposal(lcd: &str, proposal_id: u64) -> Result<()> {
    let spinner = ui::wait_spinner(&format!(
        "monitoring proposal {proposal_id} (vote in another terminal)..."
    ));
    loop {
        let proposal = lcd_query_proposal(lcd, proposal_id).await?;
        let status = proposal["status"].as_str().unwrap_or("UNKNOWN");
        spinner.set_message(format!(
            "proposal {proposal_id}: {status}{}",
            voting_eta_suffix(&proposal)
        ));

        match status {
            "PROPOSAL_STATUS_PASSED" => {
                spinner.finish_and_clear();
                ui::success(&format!("proposal {proposal_id} passed"));
                return Ok(());
            }
            "PROPOSAL_STATUS_REJECTED" | "PROPOSAL_STATUS_FAILED" => {
                spinner.finish_and_clear();
                return Err(proposal_failure(proposal_id, status, &proposal));
            }
            _ => tokio::time::sleep(COSMOS_PROPOSAL_POLL_INTERVAL).await,
        }
    }
}

/// A " — ends in 3m07s (~14:22:31)" suffix for a voting-period proposal,
/// derived from `voting_end_time`. Empty when the field is absent/unparseable.
fn voting_eta_suffix(proposal: &Value) -> String {
    let Some(end) = proposal["voting_end_time"].as_str() else {
        return String::new();
    };
    let Ok(end_dt) = chrono::DateTime::parse_from_rfc3339(end) else {
        return String::new();
    };
    let remaining = end_dt.timestamp() - chrono::Utc::now().timestamp();
    if remaining <= 0 {
        return " — voting closed, tallying".to_string();
    }
    let local = end_dt.with_timezone(&chrono::Local).format("%H:%M:%S");
    format!(" — ends in {} (~{local})", human_secs(remaining))
}

fn human_secs(total: i64) -> String {
    let minutes = total / 60;
    let seconds = total % 60;
    if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn proposal_failure(proposal_id: u64, status: &str, proposal: &Value) -> eyre::Report {
    let reason = proposal["failed_reason"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("no reason provided");
    let tally = &proposal["final_tally_result"];
    eyre::eyre!(
        "proposal {proposal_id} {status}\n  reason: {reason}\n  tally: yes={} no={} abstain={} no_with_veto={}",
        tally["yes_count"].as_str().unwrap_or("?"),
        tally["no_count"].as_str().unwrap_or("?"),
        tally["abstain_count"].as_str().unwrap_or("?"),
        tally["no_with_veto_count"].as_str().unwrap_or("?"),
    )
}

/// Fallback gov deposits (base-denom micro-units) when the config omits them:
/// expedited proposals require the larger deposit, standard the smaller.
const DEFAULT_DEPOSIT: &str = "2000000000";
const DEFAULT_EXPEDITED_DEPOSIT: &str = "3000000000";

fn parse_address(raw: &str, what: &str) -> Result<Address> {
    Address::from_str(raw).wrap_err_with(|| format!("invalid {what} address '{raw}'"))
}

fn u256_to_u64(value: U256) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_pause_calldata_encodes_bool_and_selector() {
        let on = encode_set_pause(true);
        let off = encode_set_pause(false);
        // 4-byte selector + one 32-byte ABI bool word
        assert_eq!(on.len(), 36);
        assert_eq!(&on[..4], setPauseStatusCall::SELECTOR.as_slice());
        assert_eq!(on[35], 1);
        assert_eq!(off[35], 0);
    }

    #[test]
    fn governance_payload_round_trips() {
        let target = Address::from([0x11; 20]);
        let calldata: Bytes = vec![0xaa, 0xbb].into();
        let payload = encode_governance_payload(2, target, calldata.clone(), 0);
        let (command, decoded_target, decoded_calldata, native, eta) =
            <(U256, Address, Bytes, U256, U256)>::abi_decode_params(&payload).unwrap();
        assert_eq!(command, U256::from(2));
        assert_eq!(decoded_target, target);
        assert_eq!(decoded_calldata, calldata);
        assert_eq!(native, U256::ZERO);
        assert_eq!(eta, U256::ZERO);
    }
}
