use std::collections::HashMap;

use comfy_table::{Cell, ContentArrangement, Table};
use eyre::Result;
use owo_colors::OwoColorize;
use serde::Deserialize;

use crate::cli::resolve_axelar_id;
use crate::state::{StepStatus, next_pending_step, read_state};
use crate::ui;

/// Slice of the target chains-config JSON read by the status command.
/// Only the per-chain `contracts.<name>.address` lookup is needed; everything
/// else stays untyped via the absence of the field on this struct.
#[derive(Deserialize)]
struct TargetChains {
    chains: HashMap<String, TargetChain>,
}

#[derive(Deserialize)]
struct TargetChain {
    contracts: Option<HashMap<String, TargetContract>>,
}

#[derive(Deserialize)]
struct TargetContract {
    address: Option<String>,
}

pub async fn run(axelar_id: Option<String>) -> Result<()> {
    let axelar_id = resolve_axelar_id(axelar_id)?;
    let state = read_state(&axelar_id).await?;

    ui::section(&format!("Status: {axelar_id}"));
    ui::kv("environment", state.env.as_str());
    ui::kv("rpc", &state.rpc_url);

    let target_json = &state.target_json;
    let target = tokio::fs::read_to_string(target_json)
        .await
        .ok()
        .and_then(|content| serde_json::from_str::<TargetChains>(&content).ok());
    let read_addr = |contract_name: &str| -> Option<String> {
        target
            .as_ref()?
            .chains
            .get(&axelar_id)?
            .contracts
            .as_ref()?
            .get(contract_name)?
            .address
            .clone()
    };

    let next_idx = next_pending_step(&state).map(|(idx, _)| idx);

    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec![
        Cell::new("#"),
        Cell::new("Step"),
        Cell::new("Status"),
        Cell::new("Address"),
    ]);

    for (i, step) in state.steps.iter().enumerate() {
        let is_next = Some(i) == next_idx;
        let status_str = match (step.status, is_next) {
            (StepStatus::Completed, _) => format!("{}", "+ done".green()),
            (_, true) => format!("{}", "> next".cyan().bold()),
            _ => format!("{}", "  pending".dimmed()),
        };

        let addr = if step.status == StepStatus::Completed {
            match step.name.as_str() {
                "ConstAddressDeployer"
                | "Create3Deployer"
                | "AxelarGateway"
                | "Operators"
                | "AxelarGasService" => read_addr(&step.name),
                "PredictGatewayAddress" => state.predicted_gateway_address.map(|a| format!("{a}")),
                _ => None,
            }
        } else {
            None
        };

        table.add_row(vec![
            Cell::new(i + 1),
            Cell::new(&step.name),
            Cell::new(status_str),
            Cell::new(addr.unwrap_or_default()),
        ]);
    }

    println!();
    println!("{table}");

    match next_pending_step(&state) {
        Some((_, step)) => {
            println!();
            ui::kv("next step", &step.name);
        }
        None => {
            println!();
            ui::success("all steps completed!");
        }
    }

    Ok(())
}
