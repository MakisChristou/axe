use std::path::Path;

use comfy_table::{Cell, ContentArrangement, Table};
use eyre::Result;
use serde_json::{Value, json};

use crate::config_source;
use crate::cosmos::{lcd_cosmwasm_smart_query, read_axelar_config, read_axelar_contract_field};
use crate::types::Network;
use crate::ui;

// source: axelar-skills/verifiers.md

const TESTNET_VERIFIERS: &[(&str, &str)] = &[
    ("axelar12umz2ds9gvtnkkmcwhukl7lm5asxjc9533dkj8", "Bharvest"),
    ("axelar12uqmh4qkax6ct0dr67c0ffurplwhrv7h5t9x42", "Qubelabs"),
    (
        "axelar14eh260ptse8qsk80ztmeyua9qklhccyv62h9yw",
        "Cosmostation",
    ),
    (
        "axelar16dxsfhyegy40e4eqfxee5jw5gyy2xxtcw4t2na",
        "Bokoblinet",
    ),
    ("axelar189twvmrax309e7hvke0zjgn5p55avy5ukafhc2", "Bwarelabs"),
    ("axelar19l32d5nhhwnwemzfd788j4ld095a3n6k05mmry", "Enigma"),
    (
        "axelar19xvkln5jypz8k0x9sq66mmzawkshqxfvl9h5y8",
        "ContributionDAO",
    ),
    (
        "axelar1aeylef34xqhrxn4mf8hpl94cya0rww9ld3ymep",
        "Brightlystake",
    ),
    ("axelar1afj2uhx69pjclgcspfufj9dq9x87zfv0avf6we", "4SV"),
    ("axelar1avayd50dt4mneu6s2s03yuwlc0swwmmzvf7f9f", "Redbooker"),
    ("axelar1awmhk4xhzh3224ydnrwxthpkz00tf7d5hw5kzk", "LunaNova"),
    ("axelar1ed7zk4g6rmlph6z00p6swky65qyldxrpxw9759", "AutoStake"),
    ("axelar1ejv5td70estc7ed4avnxnqqv4tpef2zafdkgms", "Quantnode"),
    (
        "axelar1gc40fw08ee4vamhvtgcszladfsrd8tyhc75l3j",
        "Encapsulate",
    ),
    ("axelar1j3u6kd4027wln9vnvmg449hmc3xj2m2g5uh69q", "Stakin"),
    (
        "axelar1j9w5c54z5erz2awtkmztfqlues9d329x5fqps0",
        "Validatrium",
    ),
    (
        "axelar1l65q24tc9e8z4dj8wj6g7t08reztazf5ur6ux2",
        "Inter Blockchain Services",
    ),
    ("axelar1lg0d9rt4syalck9ux0hhmeeayq7njmjjdguxd6", "AlexZ"),
    ("axelar1lpseq7mscuag7j9yehxmgdxh6k4ehe4hgfvfgw", "Figment"),
    (
        "axelar1melmdxuzk5mzs252kvykcjw2vyrqmqnke0mdyx",
        "P-OPS Team",
    ),
    ("axelar1pcdufjvqegu5dfqr7w4ltlfjvnpf403gt5h99n", "Nodiums"),
    ("axelar1u37w5l93vx8uts5eazm8w489h9q22k026dklaq", "DSRV"),
    (
        "axelar1verw7xy2cwhwhq6c3df0alyfxr2pl7jgy7pv5e",
        "Rockaway Infra",
    ),
    (
        "axelar1wkvh8zavznfcmsapdzxxuf2pntvktf8vzkknwa",
        "Chainlayer",
    ),
    ("axelar1wue2mm6xqk52wpynuqjlzwwux4kp3dkva5dpzw", "Liquify"),
    ("axelar1y2a43qhk7clgy0aa8fuul8746mqed379kv84u6", "Obase.vc"),
    (
        "axelar1y5dkjhyeuqmkhq42wydaxvjt8j00d86t4xnjsu",
        "Node.monster",
    ),
    ("axelar1yf58f0xkgu65stqlgf99nhmqfuzc84w2qme92m", "Imperator"),
    ("TODO", "Polkachu"),
    ("TODO", "Bitszn"),
];

const MAINNET_VERIFIERS: &[(&str, &str)] = &[
    ("axelar15k8d4hqgytdxmcx3lhph2qagvt0r7683cchglj", "Stakin"),
    (
        "axelar16g3c4z0dx3qcplhqfln92p20mkqdj9cr0wyrsh",
        "Cosmostation",
    ),
    ("axelar16ulxkme882pcwpp43rtmz7cxn95x9cqalmas5h", "Obase.vc"),
    (
        "axelar18mrzfgk63sv455c84gx0p70kl2e329gxnsmgsu",
        "Chainlayer",
    ),
    (
        "axelar19f26mhy2x488my9pc6wr5x74t4gde8l8scq34g",
        "Blockhunter",
    ),
    ("axelar1d8xyrpwpqgp9m2xuaa8gwhgraqvq8y5unv924h", "LunaNova"),
    (
        "axelar1dqqeuwvpvn2dr7gw7clayshzdemgu7j9cluehl",
        "ContributionDAO",
    ),
    ("axelar1ensvyl4p5gkdmjcezgjd5se5ykxmdqagl67xgm", "Liquify"),
    (
        "axelar1eu4zvmhum66mz7sd82sfnp6w2vfqj06gd4t8f5",
        "Validatrium",
    ),
    ("axelar1g92hckcernmgm60tm527njl6j2cxysm7zg6ulk", "Quantnode"),
    ("axelar1hm3qzhevpsfpkxnwz89j9eu6fy8lf36sl6nsd8", "Enigma"),
    (
        "axelar1kaeq00sgqvy65sngedc8dqwxerqzsg2xf7e72z",
        "Node.monster",
    ),
    ("axelar1kr5f2wrq9l2denmvfqfky7f8rd07wk9kygxjak", "Redbooker"),
    ("axelar1lkg5zs5zgywc0ua9mpd9d63gdnl3ka9n07r5fg", "DSRV"),
    (
        "axelar1nppclnu328tgvxyvu0fmd6yder3r9mrrgusrj3",
        "Encapsulate",
    ),
    ("axelar1p0z7ff4wru5yq0v2ny5h6vx5e6ceg06kqnhfpg", "axelar1"),
    ("axelar1qgwu4jjgeapqm82w4nslhwlzxa3mjd8fvn4xdx", "AlexZ"),
    (
        "axelar1s2cf963rm0u6kxgker95dh5urmq0utqq3rezdn",
        "Inter Blockchain Services",
    ),
    ("axelar1up6evve8slwnflmx0x096klxqh4ufaahsk9y0s", "Qubelabs"),
    (
        "axelar1uu6hl8uvkxjzwpuacaxwvh7ph3qjyragk62n2e",
        "P-OPS Team",
    ),
    ("axelar1wuckkey0xug0547lr3pwnuag79zpns5xt49j9a", "Figment"),
    (
        "axelar1x0a0ylzsjrr57v2ymnsl0d770nt3pwktet9npg",
        "Rockaway Infra",
    ),
    ("axelar1x9qfct58w0yxecmc294k0z39j8fqpa6nzhwwas", "AutoStake"),
    ("axelar1ym6xeu9xc8gfu5vh40a0httefxe63j537x5rle", "Nodiums"),
    (
        "axelar1zhazt54ewqhva5pujhfyhr7sf39hm7myatmjtd",
        "Brightlystake",
    ),
    ("axelar1zqnwrhv35cyf65u0059a8rvw8njtqeqjckzhlx", "Polkachu"),
    ("axelar1k22ud8g8k7dqx4u5a77gklf6f6exth0u474vt2", "Imperator"),
    ("axelar1k4whz7vj0jurjlwmu3rnx7gfanme8wx4lhzecu", "axelar2"),
];

const SUPPORTED_NETWORKS: &[crate::types::Network] = &[
    crate::types::Network::Testnet,
    crate::types::Network::Mainnet,
];

fn verifiers_for_network(network: Network) -> Result<&'static [(&'static str, &'static str)]> {
    match network {
        Network::Testnet => Ok(TESTNET_VERIFIERS),
        Network::Mainnet => Ok(MAINNET_VERIFIERS),
        _ => Err(eyre::eyre!(
            "verifier mapping only available for: {}. \
             Other networks (devnet-amplifier, stagenet) are internally operated.",
            SUPPORTED_NETWORKS
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Look up a verifier's friendly name by address on a given network. Returns
/// `None` for unknown addresses or networks without a hardcoded mapping.
pub fn lookup_name(network: Network, addr: &str) -> Option<&'static str> {
    let table = match network {
        Network::Testnet => TESTNET_VERIFIERS,
        Network::Mainnet => MAINNET_VERIFIERS,
        _ => return None,
    };
    table
        .iter()
        .find(|(a, _)| *a == addr)
        .map(|(_, name)| *name)
}

async fn resolve_chain_axelar_id(config_path: &Path, chain_input: &str) -> Result<String> {
    let content = tokio::fs::read_to_string(config_path).await?;
    let root: Value = serde_json::from_str(&content)?;
    let chains = root
        .get("chains")
        .and_then(|v| v.as_object())
        .ok_or_else(|| eyre::eyre!("no 'chains' in config"))?;

    // Exact match on chain key
    if let Some(chain_config) = chains.get(chain_input) {
        return Ok(chain_config
            .get("axelarId")
            .and_then(|v| v.as_str())
            .unwrap_or(chain_input)
            .to_string());
    }

    // Case-insensitive match on axelarId
    for (key, chain_config) in chains {
        let axelar_id = chain_config
            .get("axelarId")
            .and_then(|v| v.as_str())
            .unwrap_or(key);
        if axelar_id.eq_ignore_ascii_case(chain_input) {
            return Ok(axelar_id.to_string());
        }
    }

    let mut available: Vec<&str> = chains.keys().map(|k| k.as_str()).collect();
    available.sort();
    Err(eyre::eyre!(
        "chain '{}' not found in config. Available: {}",
        chain_input,
        available.join(", ")
    ))
}

struct ActiveVerifier {
    address: String,
    weight: String,
}

struct VerifierSet {
    active: Vec<ActiveVerifier>,
    pre_registered: Vec<String>,
    pre_registration_only: bool,
}

/// Per-verifier registration query (fallback when the active set isn't formed yet).
async fn query_verifier_supports_chain(
    lcd: &str,
    service_registry: &str,
    verifier: &str,
    chain: &str,
) -> Result<bool> {
    let q = json!({
        "verifier": { "service_name": "amplifier", "verifier": verifier }
    });
    let data = lcd_cosmwasm_smart_query(lcd, service_registry, &q).await?;
    let chains = data
        .get("supported_chains")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(chains
        .iter()
        .any(|c| c.as_str().is_some_and(|s| s == chain)))
}

fn truncate_address(addr: &str) -> String {
    if addr.len() > 20 {
        format!("{}...{}", &addr[..12], &addr[addr.len() - 6..])
    } else {
        addr.to_string()
    }
}

async fn query_verifier_set(
    lcd: &str,
    service_registry: &str,
    chain: &str,
    known_verifiers: &[(&str, &str)],
) -> Result<VerifierSet> {
    let spinner = ui::wait_spinner("querying ServiceRegistry...");
    let query = json!({
        "active_verifiers": {
            "service_name": "amplifier",
            "chain_name": chain
        }
    });
    let data = lcd_cosmwasm_smart_query(lcd, service_registry, &query).await?;
    spinner.finish_and_clear();
    if let Some(active_list) = data.as_array() {
        let active = active_list
            .iter()
            .map(|entry| ActiveVerifier {
                address: entry
                    .pointer("/verifier_info/address")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                weight: entry
                    .get("weight")
                    .and_then(Value::as_str)
                    .unwrap_or("0")
                    .to_string(),
            })
            .collect();
        return Ok(VerifierSet {
            active,
            pre_registered: Vec::new(),
            pre_registration_only: false,
        });
    }

    let spinner = ui::wait_spinner("active set not formed; checking per-verifier registrations...");
    let mut pre_registered = Vec::new();
    for (address, _) in known_verifiers {
        if *address != "TODO"
            && query_verifier_supports_chain(lcd, service_registry, address, chain)
                .await
                .unwrap_or(false)
        {
            pre_registered.push((*address).to_string());
        }
    }
    spinner.finish_and_clear();
    Ok(VerifierSet {
        active: Vec::new(),
        pre_registered,
        pre_registration_only: true,
    })
}

fn sorted_verifiers<'a>(known_verifiers: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut sorted = known_verifiers.to_vec();
    sorted.sort_by_key(|(_, name)| *name);
    sorted
}

/// The verifier rows as data.
///
/// Shared by `--json` and the MCP server so both front ends report exactly
/// the same shape and cannot drift apart.
fn verifiers_json(known_verifiers: &[(&str, &str)], verifier_set: &VerifierSet) -> Vec<Value> {
    sorted_verifiers(known_verifiers)
        .into_iter()
        .map(|(address, name)| {
            let active = verifier_set
                .active
                .iter()
                .find(|verifier| verifier.address == address);
            let registered = verifier_set
                .pre_registered
                .iter()
                .any(|registered| registered == address);
            json!({
                "name": name,
                "address": address,
                "active": active.is_some(),
                "registered": active.is_some() || registered,
                "weight": active.map(|verifier| verifier.weight.as_str()).unwrap_or("-"),
                "pre_registration_only": verifier_set.pre_registration_only,
            })
        })
        .collect::<Vec<_>>()
}

fn verifier_status(
    address: &str,
    active: Option<&ActiveVerifier>,
    pre_registered: bool,
) -> (&'static str, String, String) {
    if address == "TODO" {
        return ("? (no address)", "-".to_string(), "TODO".to_string());
    }
    let status = if active.is_some() {
        "Active"
    } else if pre_registered {
        "Registered"
    } else {
        "---"
    };
    let weight = active
        .map(|verifier| verifier.weight.clone())
        .unwrap_or_else(|| "-".to_string());
    (status, weight, truncate_address(address))
}

fn print_verifiers_table(
    chain: &str,
    known_verifiers: &[(&str, &str)],
    verifier_set: &VerifierSet,
) {
    let mut table = Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["#", "Name", "Address", "Status", "Weight"]);
    for (index, (address, name)) in sorted_verifiers(known_verifiers).into_iter().enumerate() {
        let active = verifier_set
            .active
            .iter()
            .find(|verifier| verifier.address == address);
        let pre_registered = verifier_set
            .pre_registered
            .iter()
            .any(|registered| registered == address);
        let (status, weight, display_address) = verifier_status(address, active, pre_registered);
        table.add_row(vec![
            Cell::new(index + 1),
            Cell::new(name),
            Cell::new(display_address),
            Cell::new(status),
            Cell::new(weight),
        ]);
    }
    println!("\n{table}\n");

    let known_active = verifier_set
        .active
        .iter()
        .filter(|verifier| {
            known_verifiers
                .iter()
                .any(|(address, _)| *address == verifier.address)
        })
        .count();
    if verifier_set.pre_registration_only {
        ui::kv(
            "active",
            &format!("0 verifiers for {chain} (active set not yet formed)"),
        );
        ui::kv(
            "registered",
            &format!(
                "{}/{} known verifiers have pre-registered for {chain}",
                verifier_set.pre_registered.len(),
                known_verifiers.len()
            ),
        );
    } else {
        ui::kv(
            "active",
            &format!("{} verifiers for {chain}", verifier_set.active.len()),
        );
        ui::kv(
            "known",
            &format!(
                "{known_active}/{} known verifiers are active",
                known_verifiers.len()
            ),
        );
    }
}

/// Query the verifier set for a chain, resolving the chain's axelar id on the
/// way. Prints nothing, so both front ends can share it.
async fn query(
    network: Network,
    chain: &str,
) -> Result<(String, &'static [(&'static str, &'static str)], VerifierSet)> {
    let known_verifiers = verifiers_for_network(network)?;
    let config_path = config_source::resolve(network, None).await?.into_path();
    let chain_axelar_id = resolve_chain_axelar_id(&config_path, chain).await?;

    let (lcd, _chain_id, _fee_denom, _gas_price) = read_axelar_config(&config_path).await?;
    let service_registry_addr =
        read_axelar_contract_field(&config_path, "/axelar/contracts/ServiceRegistry/address")
            .await?;

    let verifier_set = query_verifier_set(
        &lcd,
        &service_registry_addr,
        &chain_axelar_id,
        known_verifiers,
    )
    .await?;

    if verifier_set.pre_registration_only && verifier_set.pre_registered.is_empty() {
        return Err(eyre::eyre!(
            "no verifiers found for chain '{chain_axelar_id}' on {network}. \
             Is it an Amplifier chain?"
        ));
    }

    Ok((chain_axelar_id, known_verifiers, verifier_set))
}

/// The active verifier set for a chain, as data.
pub(crate) async fn resolve(network: Network, chain: &str) -> Result<Value> {
    let (chain_axelar_id, known_verifiers, verifier_set) = query(network, chain).await?;
    Ok(json!({
        "network": network.to_string(),
        "chain": chain_axelar_id,
        "verifiers": verifiers_json(known_verifiers, &verifier_set),
    }))
}

pub async fn run(network: Network, chain: String, json_mode: bool) -> Result<()> {
    let (chain_axelar_id, known_verifiers, verifier_set) = query(network, &chain).await?;

    // Printed after the query rather than before it, so the shared query can
    // stay silent. Same text as before, just emitted once the answer is in.
    if !json_mode {
        ui::section(&format!("Verifiers: {network} / {chain_axelar_id}"));
    }

    if json_mode {
        let entries = verifiers_json(known_verifiers, &verifier_set);
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    print_verifiers_table(&chain_axelar_id, known_verifiers, &verifier_set);
    Ok(())
}
