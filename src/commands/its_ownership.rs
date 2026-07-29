use std::io::IsTerminal;
use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use comfy_table::{Attribute, Cell, Color, ContentArrangement, Table};
use eyre::{Result, WrapErr};
use futures::stream::{self, StreamExt};
use serde_json::{Value, json};
use stellar_strkey::ed25519::PublicKey as StellarPublicKey;

use crate::config::{ChainConfig, ChainsConfig, ContractEntry};
use crate::config_source;
use crate::evm::InterchainTokenService;
use crate::stellar::{StellarClient, scval_to_address_string};
use crate::sui::SuiClient;
use crate::types::Network;
use crate::ui;

const ITS_CONTRACT: &str = "InterchainTokenService";
const QUERY_CONCURRENCY: usize = 8;
const QUERY_TIMEOUT: Duration = Duration::from_secs(12);
const GOVERNANCE_CONTRACTS: [(&str, &str); 3] = [
    ("AxelarServiceGovernance", "AxelarServiceGov"),
    ("InterchainGovernance", "InterchainGov"),
    ("AxelarGovernance", "AxelarGov"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItsChainKind {
    Evm,
    Solana,
    Sui,
    Stellar,
}

impl ItsChainKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Evm => "EVM",
            Self::Solana => "Solana",
            Self::Sui => "Sui",
            Self::Stellar => "Stellar",
        }
    }

    const fn config_label(self) -> &'static str {
        match self {
            Self::Evm => "evm",
            Self::Solana => "svm",
            Self::Sui => "sui",
            Self::Stellar => "stellar",
        }
    }

    const fn sort_rank(self) -> u8 {
        match self {
            Self::Evm => 0,
            Self::Solana => 1,
            Self::Sui => 2,
            Self::Stellar => 3,
        }
    }
}

#[derive(Clone, Debug)]
struct GovernanceContract {
    name: &'static str,
    label: &'static str,
    address: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum GovernanceStatus {
    Owner(String),
    Deployed(String),
    NotDeployed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum OwnerKind {
    Governance(String),
    Eoa,
    Contract,
    Account,
    Unknown,
    Missing,
}

impl OwnerKind {
    fn label(&self) -> String {
        match self {
            Self::Governance(label) => format!("gov: {label}"),
            Self::Eoa => "EOA".to_string(),
            Self::Contract => "contract".to_string(),
            Self::Account => "account".to_string(),
            Self::Unknown => "unknown".to_string(),
            Self::Missing => "missing".to_string(),
        }
    }

    const fn json_label(&self) -> &'static str {
        match self {
            Self::Governance(_) => "governance_contract",
            Self::Eoa => "eoa",
            Self::Contract => "contract",
            Self::Account => "account",
            Self::Unknown => "unknown",
            Self::Missing => "missing",
        }
    }

    const fn color(&self) -> Color {
        match self {
            Self::Governance(_) => Color::Green,
            Self::Eoa => Color::Yellow,
            Self::Contract | Self::Account => Color::Cyan,
            Self::Unknown => Color::DarkGrey,
            Self::Missing => Color::Red,
        }
    }
}

impl GovernanceStatus {
    fn label(&self) -> String {
        match self {
            Self::Owner(label) => format!("owner: {label}"),
            Self::Deployed(label) => format!("deployed: {label}"),
            Self::NotDeployed => "not deployed".to_string(),
        }
    }

    const fn is_deployed(&self) -> bool {
        !matches!(self, Self::NotDeployed)
    }

    const fn is_owner(&self) -> bool {
        matches!(self, Self::Owner(_))
    }

    const fn json_status(&self) -> &'static str {
        match self {
            Self::Owner(_) => "owner",
            Self::Deployed(_) => "deployed",
            Self::NotDeployed => "not_deployed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldSource {
    OnChain,
    Config,
    Missing,
}

impl FieldSource {
    const fn label(self) -> &'static str {
        match self {
            Self::OnChain => "on_chain",
            Self::Config => "config",
            Self::Missing => "missing",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddressRole {
    Its,
    Owner,
    Operator,
    Governance,
}

#[derive(Clone, Debug)]
struct AddressLink {
    label: String,
    url: String,
}

#[derive(Clone, Debug)]
struct ItsEntry {
    chain_key: String,
    axelar_id: String,
    kind: ItsChainKind,
    rpc_url: Option<String>,
    explorer_url: Option<String>,
    address: String,
    version: Option<String>,
    deployer: Option<String>,
    config_owner: Option<String>,
    config_operator: Option<String>,
    upgrade_authority: Option<String>,
    sui_owner_cap: Option<String>,
    sui_operator_cap: Option<String>,
    stellar_network_type: String,
    governance_contracts: Vec<GovernanceContract>,
}

impl ItsEntry {
    fn from_config(
        chain_key: &str,
        chain: &ChainConfig,
        contract: &ContractEntry,
        kind: ItsChainKind,
        network: Network,
    ) -> Option<Self> {
        let address = contract_string(contract, "address")?;
        let config_owner = contract_string(contract, "owner")
            .or_else(|| contract_path_string(contract, &["initializeArgs", "owner"]));
        let config_operator = contract_string(contract, "operator")
            .or_else(|| contract_path_string(contract, &["initializeArgs", "operator"]));

        Some(Self {
            chain_key: chain_key.to_string(),
            axelar_id: chain.axelar_id_or(chain_key),
            kind,
            rpc_url: non_empty(chain.rpc.clone()),
            explorer_url: explorer_url(chain),
            address,
            version: contract_string(contract, "version"),
            deployer: contract_string(contract, "deployer"),
            config_owner,
            config_operator,
            upgrade_authority: contract_string(contract, "upgradeAuthority"),
            sui_owner_cap: contract_path_string(contract, &["objects", "OwnerCap"]),
            sui_operator_cap: contract_path_string(contract, &["objects", "OperatorCap"]),
            stellar_network_type: stellar_network_type(chain, network),
            governance_contracts: governance_contracts(chain),
        })
    }
}

type FieldResult = std::result::Result<String, String>;

struct OwnershipProbe {
    owner: FieldResult,
    operator: FieldResult,
    owner_has_code: Option<std::result::Result<bool, String>>,
}

impl OwnershipProbe {
    fn both_err(message: String) -> Self {
        Self {
            owner: Err(message.clone()),
            operator: Err(message),
            owner_has_code: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnershipStatus {
    OnChain,
    Partial,
    Config,
    Missing,
}

impl OwnershipStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::OnChain => "on_chain",
            Self::Partial => "partial",
            Self::Config => "config",
            Self::Missing => "missing",
        }
    }
}

#[derive(Clone, Debug)]
struct OwnershipRow {
    chain_key: String,
    axelar_id: String,
    kind: ItsChainKind,
    its_address: String,
    explorer_url: Option<String>,
    owner: Option<String>,
    owner_source: FieldSource,
    operator: Option<String>,
    operator_source: FieldSource,
    owner_kind: OwnerKind,
    governance: GovernanceStatus,
    governance_contracts: Vec<GovernanceContract>,
    version: Option<String>,
    status: OwnershipStatus,
    note: String,
}

struct OwnershipFields {
    owner: Option<String>,
    owner_source: FieldSource,
    operator: Option<String>,
    operator_source: FieldSource,
    owner_has_code: Option<std::result::Result<bool, String>>,
    status: OwnershipStatus,
    note: String,
}

pub async fn run(network: Network, json_output: bool) -> Result<()> {
    let config_path = config_source::resolve(network, None).await?.into_path();
    let config = ChainsConfig::load(&config_path)?;
    let entries = collect_its_entries(&config, network);

    if entries.is_empty() {
        return Err(eyre::eyre!(
            "no {ITS_CONTRACT} deployments found in {}",
            config_path.display()
        ));
    }

    if !json_output {
        ui::section(&format!("ITS ownership: {network}"));
        ui::kv("config", &config_path.display().to_string());
    }

    let mut rows = if json_output {
        query_entries(entries).await
    } else {
        let spinner = ui::wait_spinner(&format!(
            "querying {} ITS deployments (read-only)...",
            entries.len()
        ));
        let rows = query_entries(entries).await;
        spinner.finish_and_clear();
        rows
    };

    sort_rows(&mut rows);
    if json_output {
        render_json(network, &config_path, &rows)?;
    } else {
        render_table(&rows);
        render_summary(&rows);
    }

    Ok(())
}

async fn query_entries(entries: Vec<ItsEntry>) -> Vec<OwnershipRow> {
    stream::iter(entries)
        .map(query_entry)
        .buffer_unordered(QUERY_CONCURRENCY)
        .collect()
        .await
}

fn collect_its_entries(config: &ChainsConfig, network: Network) -> Vec<ItsEntry> {
    let mut entries = Vec::new();
    for (chain_key, chain) in &config.chains {
        let Some(contract) = chain
            .contracts
            .as_ref()
            .and_then(|contracts| contracts.get(ITS_CONTRACT))
        else {
            continue;
        };
        let Some(kind) = chain_kind(chain.chain_type.as_deref()) else {
            continue;
        };
        let Some(entry) = ItsEntry::from_config(chain_key, chain, contract, kind, network) else {
            continue;
        };
        entries.push(entry);
    }
    entries.sort_by(|a, b| {
        a.kind
            .sort_rank()
            .cmp(&b.kind.sort_rank())
            .then_with(|| a.chain_key.cmp(&b.chain_key))
    });
    entries
}

fn chain_kind(chain_type: Option<&str>) -> Option<ItsChainKind> {
    match chain_type {
        Some("evm") => Some(ItsChainKind::Evm),
        Some("svm") => Some(ItsChainKind::Solana),
        Some("sui") => Some(ItsChainKind::Sui),
        Some("stellar") => Some(ItsChainKind::Stellar),
        _ => None,
    }
}

async fn query_entry(entry: ItsEntry) -> OwnershipRow {
    match entry.kind {
        ItsChainKind::Evm => query_evm_entry(entry).await,
        ItsChainKind::Solana => solana_config_row(entry),
        ItsChainKind::Sui => query_sui_entry(entry).await,
        ItsChainKind::Stellar => query_stellar_entry(entry).await,
    }
}

async fn query_evm_entry(entry: ItsEntry) -> OwnershipRow {
    let owner_fallback = entry.config_owner.clone();
    let probe = probe_with_timeout(probe_evm(&entry)).await;
    row_from_probe(entry, probe, owner_fallback, None, "config fallback")
}

async fn probe_evm(entry: &ItsEntry) -> Result<OwnershipProbe> {
    let rpc_url = entry
        .rpc_url
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no RPC URL in config"))?;
    let its_address = Address::from_str(&entry.address)
        .wrap_err_with(|| format!("invalid EVM ITS address '{}'", entry.address))?;
    let provider = ProviderBuilder::new().connect_http(
        rpc_url
            .parse()
            .wrap_err_with(|| format!("invalid RPC URL '{rpc_url}'"))?,
    );
    let its = InterchainTokenService::new(its_address, &provider);
    let owner_call = its.owner();
    let (owner, operator) = tokio::join!(owner_call.call(), probe_evm_operator(&its, entry));
    let owner = owner
        .map(|address| address.to_string())
        .map_err(|err| err.to_string());
    let owner_for_code = owner.as_deref().ok().or(entry.config_owner.as_deref());
    let owner_has_code = match owner_for_code {
        Some(owner) => Some(query_evm_owner_has_code(&provider, owner).await),
        None => None,
    };

    Ok(OwnershipProbe {
        owner,
        operator,
        owner_has_code,
    })
}

async fn query_evm_owner_has_code<P>(provider: &P, owner: &str) -> std::result::Result<bool, String>
where
    P: alloy::providers::Provider,
{
    let address = Address::from_str(owner).map_err(|err| err.to_string())?;
    provider
        .get_code_at(address)
        .await
        .map(|code| !code.is_empty())
        .map_err(|err| err.to_string())
}

async fn probe_evm_operator<P>(
    its: &InterchainTokenService::InterchainTokenServiceInstance<P>,
    entry: &ItsEntry,
) -> FieldResult
where
    P: alloy::providers::Provider,
{
    let candidates = evm_operator_candidates(entry);
    if candidates.is_empty() {
        return Err("no operator candidate in config".to_string());
    }

    let mut last_error = None;
    for candidate in candidates {
        let Ok(address) = Address::from_str(&candidate) else {
            continue;
        };
        let call = its.isOperator(address);
        match call.call().await {
            Ok(true) => return Ok(candidate),
            Ok(false) => {}
            Err(err) => last_error = Some(err.to_string()),
        }
    }

    Err(last_error.unwrap_or_else(|| "no configured candidate has operator role".to_string()))
}

fn evm_operator_candidates(entry: &ItsEntry) -> Vec<String> {
    unique_values([
        entry.config_operator.as_deref(),
        entry.deployer.as_deref(),
        entry.config_owner.as_deref(),
    ])
}

fn solana_config_row(entry: ItsEntry) -> OwnershipRow {
    let owner = entry
        .config_owner
        .clone()
        .or(entry.upgrade_authority.clone());
    let operator = entry.config_operator.clone();
    let note = if entry.config_owner.is_some() {
        "config owner/operator".to_string()
    } else if entry.upgrade_authority.is_some() {
        "owner shown as upgradeAuthority".to_string()
    } else {
        "config only".to_string()
    };
    let status = if owner.is_some() || operator.is_some() {
        OwnershipStatus::Config
    } else {
        OwnershipStatus::Missing
    };
    let owner_source = source_for_value(owner.as_ref(), FieldSource::Config);
    let operator_source = source_for_value(operator.as_ref(), FieldSource::Config);
    OwnershipRow::from_entry(
        entry,
        OwnershipFields {
            owner,
            owner_source,
            operator,
            operator_source,
            owner_has_code: None,
            status,
            note,
        },
    )
}

async fn query_sui_entry(entry: ItsEntry) -> OwnershipRow {
    let owner_fallback = entry.config_owner.clone();
    let operator_fallback = entry.config_operator.clone();
    let probe = probe_with_timeout(probe_sui(&entry)).await;
    row_from_probe(
        entry,
        probe,
        owner_fallback,
        operator_fallback,
        "config fallback",
    )
}

async fn probe_sui(entry: &ItsEntry) -> Result<OwnershipProbe> {
    let rpc_url = entry
        .rpc_url
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no RPC URL in config"))?;
    let owner_cap = entry
        .sui_owner_cap
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no OwnerCap object in config"))?;
    let operator_cap = entry
        .sui_operator_cap
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no OperatorCap object in config"))?;
    let client = SuiClient::new(rpc_url);
    let (owner, operator) = tokio::join!(
        query_sui_object_owner(&client, owner_cap),
        query_sui_object_owner(&client, operator_cap)
    );

    Ok(OwnershipProbe {
        owner,
        operator,
        owner_has_code: None,
    })
}

async fn query_sui_object_owner(client: &SuiClient, object_id: &str) -> FieldResult {
    let result = client
        .call(
            "sui_getObject",
            json!([object_id, {"showOwner": true, "showPreviousTransaction": false}]),
        )
        .await
        .map_err(|err| err.to_string())?;
    sui_owner_from_object(&result).ok_or_else(|| format!("owner not found for object {object_id}"))
}

fn sui_owner_from_object(value: &Value) -> Option<String> {
    let owner = value.pointer("/data/owner")?;
    if let Some(address) = owner.get("AddressOwner").and_then(Value::as_str) {
        return Some(address.to_string());
    }
    owner
        .get("ObjectOwner")
        .and_then(Value::as_str)
        .map(str::to_string)
}

async fn query_stellar_entry(entry: ItsEntry) -> OwnershipRow {
    let owner_fallback = entry.config_owner.clone();
    let operator_fallback = entry.config_operator.clone();
    let probe = probe_with_timeout(probe_stellar(&entry)).await;
    row_from_probe(
        entry,
        probe,
        owner_fallback,
        operator_fallback,
        "initializeArgs fallback",
    )
}

async fn probe_stellar(entry: &ItsEntry) -> Result<OwnershipProbe> {
    let rpc_url = entry
        .rpc_url
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no RPC URL in config"))?;
    let client = StellarClient::new(rpc_url, &entry.stellar_network_type)?;
    let source = stellar_source_account(entry).unwrap_or([0; 32]);
    let (owner, operator) = tokio::join!(
        query_stellar_address_view(&client, &source, &entry.address, "owner"),
        query_stellar_address_view(&client, &source, &entry.address, "operator")
    );

    Ok(OwnershipProbe {
        owner,
        operator,
        owner_has_code: None,
    })
}

async fn query_stellar_address_view(
    client: &StellarClient,
    source: &[u8; 32],
    contract: &str,
    function: &str,
) -> FieldResult {
    let result = client
        .simulate_view(source, contract, function, Vec::new())
        .await
        .map_err(|err| err.to_string())?;
    let Some(value) = result else {
        return Err(format!("{function} returned no result"));
    };
    scval_to_address_string(&value).ok_or_else(|| format!("{function} returned non-address"))
}

fn stellar_source_account(entry: &ItsEntry) -> Option<[u8; 32]> {
    [
        entry.deployer.as_deref(),
        entry.config_owner.as_deref(),
        entry.config_operator.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(parse_stellar_account)
}

fn parse_stellar_account(address: &str) -> Option<[u8; 32]> {
    if !address.starts_with('G') {
        return None;
    }
    StellarPublicKey::from_string(address)
        .ok()
        .map(|public_key| public_key.0)
}

async fn probe_with_timeout<F>(probe: F) -> OwnershipProbe
where
    F: std::future::Future<Output = Result<OwnershipProbe>>,
{
    match tokio::time::timeout(QUERY_TIMEOUT, probe).await {
        Ok(Ok(probe)) => probe,
        Ok(Err(err)) => OwnershipProbe::both_err(err.to_string()),
        Err(_) => OwnershipProbe::both_err(format!(
            "query timed out after {}s",
            QUERY_TIMEOUT.as_secs()
        )),
    }
}

fn row_from_probe(
    entry: ItsEntry,
    probe: OwnershipProbe,
    owner_fallback: Option<String>,
    operator_fallback: Option<String>,
    fallback_note: &str,
) -> OwnershipRow {
    let owner_error = probe.owner.as_ref().err().cloned();
    let operator_error = probe.operator.as_ref().err().cloned();
    let owner_on_chain = probe.owner.ok();
    let operator_on_chain = probe.operator.ok();
    let owner_from_chain = owner_on_chain.is_some();
    let operator_from_chain = operator_on_chain.is_some();
    let owner_source = field_source(owner_from_chain, owner_fallback.is_some());
    let operator_source = field_source(operator_from_chain, operator_fallback.is_some());
    let owner = owner_on_chain.or(owner_fallback);
    let operator = operator_on_chain.or(operator_fallback);
    let status = ownership_status(owner_from_chain, operator_from_chain, &owner, &operator);
    let owner_has_code = probe.owner_has_code;
    let note = ownership_note(
        status,
        fallback_note,
        owner_error.as_deref(),
        operator_error.as_deref(),
    );

    OwnershipRow::from_entry(
        entry,
        OwnershipFields {
            owner,
            owner_source,
            operator,
            operator_source,
            owner_has_code,
            status,
            note,
        },
    )
}

fn field_source(from_chain: bool, has_fallback: bool) -> FieldSource {
    if from_chain {
        FieldSource::OnChain
    } else if has_fallback {
        FieldSource::Config
    } else {
        FieldSource::Missing
    }
}

fn source_for_value(value: Option<&String>, source: FieldSource) -> FieldSource {
    if value.is_some() {
        source
    } else {
        FieldSource::Missing
    }
}

fn ownership_status(
    owner_from_chain: bool,
    operator_from_chain: bool,
    owner: &Option<String>,
    operator: &Option<String>,
) -> OwnershipStatus {
    if owner_from_chain && operator_from_chain {
        OwnershipStatus::OnChain
    } else if owner_from_chain || operator_from_chain {
        OwnershipStatus::Partial
    } else if owner.is_some() || operator.is_some() {
        OwnershipStatus::Config
    } else {
        OwnershipStatus::Missing
    }
}

fn ownership_note(
    status: OwnershipStatus,
    fallback_note: &str,
    owner_error: Option<&str>,
    operator_error: Option<&str>,
) -> String {
    match status {
        OwnershipStatus::OnChain => String::new(),
        OwnershipStatus::Config => fallback_note.to_string(),
        OwnershipStatus::Partial | OwnershipStatus::Missing => {
            probe_error_note(owner_error, operator_error)
        }
    }
}

fn probe_error_note(owner_error: Option<&str>, operator_error: Option<&str>) -> String {
    match (owner_error, operator_error) {
        (Some(owner), Some(operator)) => {
            let owner = short_error(owner);
            let operator = short_error(operator);
            if owner == operator {
                format!("owner/operator: {owner}")
            } else {
                format!("owner: {owner}; operator: {operator}")
            }
        }
        (Some(owner), None) => format!("owner: {}", short_error(owner)),
        (None, Some(operator)) => format!("operator: {}", short_error(operator)),
        (None, None) => String::new(),
    }
}

impl OwnershipRow {
    fn from_entry(entry: ItsEntry, fields: OwnershipFields) -> Self {
        let governance = governance_status(fields.owner.as_deref(), &entry.governance_contracts);
        let owner_kind = owner_kind(
            fields.owner.as_deref(),
            entry.kind,
            &governance,
            fields.owner_has_code.as_ref(),
        );

        Self {
            chain_key: entry.chain_key,
            axelar_id: entry.axelar_id,
            kind: entry.kind,
            its_address: entry.address,
            explorer_url: entry.explorer_url,
            owner: fields.owner,
            owner_source: fields.owner_source,
            operator: fields.operator,
            operator_source: fields.operator_source,
            owner_kind,
            governance,
            governance_contracts: entry.governance_contracts,
            version: entry.version,
            status: fields.status,
            note: fields.note,
        }
    }
}

fn render_table(rows: &[OwnershipRow]) {
    let mut table = Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    let hyperlinks = terminal_hyperlinks_enabled();
    let mut links = Vec::new();
    table.set_header(vec![
        header_cell("#"),
        header_cell("Axelar ID"),
        header_cell("Type"),
        header_cell("ITS"),
        header_cell("Owner"),
        header_cell("Owner Type"),
        header_cell("Gov"),
        header_cell("Operator"),
        header_cell("Version"),
        header_cell("Note"),
    ]);

    for (index, row) in rows.iter().enumerate() {
        let owner_type = owner_type_cell(row);
        table.add_row(vec![
            Cell::new(index + 1).fg(Color::DarkGrey),
            Cell::new(row.axelar_id.as_str()).fg(Color::Cyan),
            Cell::new(row.kind.label()),
            Cell::new(address_cell(
                row,
                &row.its_address,
                AddressRole::Its,
                hyperlinks,
                &mut links,
            )),
            Cell::new(optional_address_cell(
                row,
                row.owner.as_deref(),
                AddressRole::Owner,
                hyperlinks,
                &mut links,
            )),
            Cell::new(owner_type).fg(row.owner_kind.color()),
            Cell::new(governance_address_cell(row, hyperlinks, &mut links)),
            Cell::new(optional_address_cell(
                row,
                row.operator.as_deref(),
                AddressRole::Operator,
                hyperlinks,
                &mut links,
            )),
            Cell::new(row.version.as_deref().unwrap_or("-")),
            Cell::new(row.note.as_str()).fg(Color::DarkGrey),
        ]);
    }

    println!();
    let rendered = if hyperlinks {
        overlay_terminal_links(&table.to_string(), &links)
    } else {
        table.to_string()
    };
    println!("{rendered}");
}

fn header_cell(label: &str) -> Cell {
    Cell::new(label)
        .fg(Color::Cyan)
        .add_attribute(Attribute::Bold)
}

fn render_summary(rows: &[OwnershipRow]) {
    println!();
    ui::kv("rows", &row_count_summary(rows));
    ui::kv(
        "sources",
        "EVM owner()/isOperator(config candidates); Sui cap owners; Stellar owner()/operator(); Solana config",
    );
    ui::kv("governance", &governance_summary(rows));

    let missing = rows
        .iter()
        .filter(|row| row.status == OwnershipStatus::Missing)
        .count();
    if missing > 0 {
        let noun = if missing == 1 { "row is" } else { "rows are" };
        ui::warn(&format!(
            "{missing} {noun} missing both owner and operator; check notes/RPCs"
        ));
    }
}

fn render_json(
    network: Network,
    config_path: &std::path::Path,
    rows: &[OwnershipRow],
) -> Result<()> {
    let output = json!({
        "network": network.to_string(),
        "config": config_path.display().to_string(),
        "rows": rows.iter().map(row_json).collect::<Vec<_>>(),
        "summary": {
            "rows": rows.len(),
            "evm": count_kind(rows, ItsChainKind::Evm),
            "solana": count_kind(rows, ItsChainKind::Solana),
            "sui": count_kind(rows, ItsChainKind::Sui),
            "stellar": count_kind(rows, ItsChainKind::Stellar),
            "governance": governance_summary_json(rows),
        },
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn row_json(row: &OwnershipRow) -> Value {
    json!({
        "chain": row.chain_key,
        "axelarId": row.axelar_id,
        "chainType": row.kind.config_label(),
        "displayType": row.kind.label(),
        "rpcQueryStatus": row.status.label(),
        "explorer": {
            "baseUrl": row.explorer_url,
        },
        "its": address_json(row, &row.its_address, FieldSource::Config, AddressRole::Its),
        "owner": owner_field_json(row),
        "operator": optional_field_json(
            row,
            row.operator.as_deref(),
            row.operator_source,
            AddressRole::Operator,
        ),
        "governance": {
            "status": row.governance.json_status(),
            "label": row.governance.label(),
            "deployed": row.governance.is_deployed(),
            "ownerMatches": row.governance.is_owner(),
            "contracts": row.governance_contracts
                .iter()
                .map(|contract| governance_contract_json(row, contract))
                .collect::<Vec<_>>(),
        },
        "version": row.version,
        "note": row.note,
    })
}

fn owner_field_json(row: &OwnershipRow) -> Value {
    json!({
        "address": row.owner,
        "source": row.owner_source.label(),
        "kind": row.owner_kind.json_label(),
        "kindLabel": row.owner_kind.label(),
        "explorerUrl": row.owner.as_deref().and_then(|owner| {
            address_explorer_url(row.kind, row.explorer_url.as_deref(), owner, AddressRole::Owner)
        }),
    })
}

fn address_json(
    row: &OwnershipRow,
    address: &str,
    source: FieldSource,
    role: AddressRole,
) -> Value {
    json!({
        "address": address,
        "source": source.label(),
        "explorerUrl": address_explorer_url(row.kind, row.explorer_url.as_deref(), address, role),
    })
}

fn optional_field_json(
    row: &OwnershipRow,
    address: Option<&str>,
    source: FieldSource,
    role: AddressRole,
) -> Value {
    match address {
        Some(address) => address_json(row, address, source, role),
        None => json!({
            "address": null,
            "source": source.label(),
            "explorerUrl": null,
        }),
    }
}

fn governance_contract_json(row: &OwnershipRow, contract: &GovernanceContract) -> Value {
    json!({
        "name": contract.name,
        "label": contract.label,
        "address": contract.address,
        "ownerMatches": row
            .owner
            .as_deref()
            .is_some_and(|owner| same_address(owner, &contract.address)),
        "explorerUrl": address_explorer_url(
            row.kind,
            row.explorer_url.as_deref(),
            &contract.address,
            AddressRole::Governance,
        ),
    })
}

fn governance_summary_json(rows: &[OwnershipRow]) -> Value {
    let deployed = rows
        .iter()
        .filter(|row| row.governance.is_deployed())
        .count();
    let owner_matches = rows.iter().filter(|row| row.governance.is_owner()).count();
    json!({
        "deployed": deployed,
        "notDeployed": rows.len() - deployed,
        "ownerMatches": owner_matches,
    })
}

fn row_count_summary(rows: &[OwnershipRow]) -> String {
    let evm = count_kind(rows, ItsChainKind::Evm);
    let solana = count_kind(rows, ItsChainKind::Solana);
    let sui = count_kind(rows, ItsChainKind::Sui);
    let stellar = count_kind(rows, ItsChainKind::Stellar);
    format!(
        "{} total ({} EVM, {} Solana, {} Sui, {} Stellar)",
        rows.len(),
        evm,
        solana,
        sui,
        stellar
    )
}

fn governance_summary(rows: &[OwnershipRow]) -> String {
    let deployed = rows
        .iter()
        .filter(|row| row.governance.is_deployed())
        .count();
    let owner_matches = rows.iter().filter(|row| row.governance.is_owner()).count();
    format!(
        "{deployed} deployed, {} not deployed, {owner_matches} owner match(es)",
        rows.len() - deployed
    )
}

fn count_kind(rows: &[OwnershipRow], kind: ItsChainKind) -> usize {
    rows.iter().filter(|row| row.kind == kind).count()
}

fn sort_rows(rows: &mut [OwnershipRow]) {
    rows.sort_by(|a, b| {
        a.kind
            .sort_rank()
            .cmp(&b.kind.sort_rank())
            .then_with(|| a.chain_key.cmp(&b.chain_key))
    });
}

fn contract_string(contract: &ContractEntry, key: &str) -> Option<String> {
    let value = match key {
        "address" => contract.address.clone(),
        "deployer" => contract.deployer.clone(),
        "version" => contract.version.clone(),
        _ => contract
            .extra
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    non_empty(value)
}

fn contract_path_string(contract: &ContractEntry, path: &[&str]) -> Option<String> {
    let (first, rest) = path.split_first()?;
    let mut current = contract.extra.get(*first)?;
    for key in rest {
        current = current.get(*key)?;
    }
    current
        .as_str()
        .and_then(|value| non_empty(Some(value.to_string())))
}

fn explorer_url(chain: &ChainConfig) -> Option<String> {
    chain
        .extra
        .get("explorer")
        .and_then(|explorer| explorer.get("url"))
        .and_then(Value::as_str)
        .and_then(|value| non_empty(Some(value.to_string())))
}

fn governance_contracts(chain: &ChainConfig) -> Vec<GovernanceContract> {
    let Some(contracts) = chain.contracts.as_ref() else {
        return Vec::new();
    };

    GOVERNANCE_CONTRACTS
        .iter()
        .filter_map(|(contract_name, label)| {
            let address = contracts
                .get(*contract_name)
                .and_then(|contract| contract_string(contract, "address"))?;
            Some(GovernanceContract {
                name: contract_name,
                label,
                address,
            })
        })
        .collect()
}

fn matching_governance(
    owner: Option<&str>,
    governance_contracts: &[GovernanceContract],
) -> Option<String> {
    let owner = owner?;
    let matches = governance_contracts
        .iter()
        .filter(|contract| same_address(owner, &contract.address))
        .map(|contract| contract.label)
        .collect::<Vec<_>>();
    if matches.is_empty() {
        None
    } else {
        Some(matches.join(", "))
    }
}

fn governance_status(
    owner: Option<&str>,
    governance_contracts: &[GovernanceContract],
) -> GovernanceStatus {
    if governance_contracts.is_empty() {
        return GovernanceStatus::NotDeployed;
    }

    let labels = governance_labels(governance_contracts);
    if let Some(matches) = matching_governance(owner, governance_contracts) {
        GovernanceStatus::Owner(matches)
    } else {
        GovernanceStatus::Deployed(labels)
    }
}

fn owner_kind(
    owner: Option<&str>,
    kind: ItsChainKind,
    governance: &GovernanceStatus,
    owner_has_code: Option<&std::result::Result<bool, String>>,
) -> OwnerKind {
    let Some(_) = owner else {
        return OwnerKind::Missing;
    };

    if let GovernanceStatus::Owner(label) = governance {
        return OwnerKind::Governance(label.clone());
    }

    match kind {
        ItsChainKind::Evm => match owner_has_code {
            Some(Ok(false)) => OwnerKind::Eoa,
            Some(Ok(true)) => OwnerKind::Contract,
            Some(Err(_)) | None => OwnerKind::Unknown,
        },
        ItsChainKind::Solana | ItsChainKind::Sui | ItsChainKind::Stellar => OwnerKind::Account,
    }
}

fn owner_type_cell(row: &OwnershipRow) -> String {
    if row.owner_kind == OwnerKind::Missing {
        return "-".to_string();
    }

    let label = row.owner_kind.label();
    if matches!(
        row.owner_kind,
        OwnerKind::Governance(_) | OwnerKind::Unknown
    ) {
        return label;
    }

    if row.governance.is_deployed() {
        format!("{label}; gov deployed")
    } else {
        format!("{label}; no gov")
    }
}

fn governance_address_cell(
    row: &OwnershipRow,
    hyperlinks: bool,
    links: &mut Vec<AddressLink>,
) -> String {
    if row.governance_contracts.is_empty() {
        return "-".to_string();
    }

    row.governance_contracts
        .iter()
        .map(|contract| {
            address_cell(
                row,
                &contract.address,
                AddressRole::Governance,
                hyperlinks,
                links,
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn governance_labels(governance_contracts: &[GovernanceContract]) -> String {
    governance_contracts
        .iter()
        .map(|contract| contract.label)
        .collect::<Vec<_>>()
        .join(", ")
}

fn same_address(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

fn stellar_network_type(chain: &ChainConfig, network: Network) -> String {
    chain
        .extra
        .get("networkType")
        .and_then(Value::as_str)
        .and_then(|value| non_empty(Some(value.to_string())))
        .unwrap_or_else(|| match network {
            Network::Mainnet => "mainnet".to_string(),
            Network::Testnet | Network::Stagenet | Network::DevnetAmplifier => {
                "testnet".to_string()
            }
        })
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|inner| {
        let trimmed = inner.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn unique_values(values: [Option<&str>; 3]) -> Vec<String> {
    let mut unique = Vec::new();
    for value in values.into_iter().flatten() {
        if !unique.iter().any(|existing| existing == value) {
            unique.push(value.to_string());
        }
    }
    unique
}

fn compact_address(address: &str) -> String {
    const HEAD: usize = 10;
    const TAIL: usize = 6;
    if address.len() <= HEAD + TAIL + 3 {
        return address.to_string();
    }
    format!(
        "{}...{}",
        &address[..HEAD],
        &address[address.len() - TAIL..]
    )
}

fn optional_address_cell(
    row: &OwnershipRow,
    address: Option<&str>,
    role: AddressRole,
    hyperlinks: bool,
    links: &mut Vec<AddressLink>,
) -> String {
    address
        .map(|address| address_cell(row, address, role, hyperlinks, links))
        .unwrap_or_else(|| "-".to_string())
}

fn address_cell(
    row: &OwnershipRow,
    address: &str,
    role: AddressRole,
    hyperlinks: bool,
    links: &mut Vec<AddressLink>,
) -> String {
    let label = compact_address(address);
    if !hyperlinks {
        return label;
    }

    let Some(url) = address_explorer_url(row.kind, row.explorer_url.as_deref(), address, role)
    else {
        return label;
    };
    links.push(AddressLink {
        label: label.clone(),
        url,
    });
    label
}

fn overlay_terminal_links(rendered: &str, links: &[AddressLink]) -> String {
    let mut output = String::with_capacity(rendered.len());
    let mut cursor = 0;

    for link in links {
        let Some(relative_position) = rendered[cursor..].find(&link.label) else {
            continue;
        };
        let position = cursor + relative_position;
        output.push_str(&rendered[cursor..position]);
        output.push_str(&terminal_link(&link.label, &link.url));
        cursor = position + link.label.len();
    }

    output.push_str(&rendered[cursor..]);
    output
}

fn terminal_link(label: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\")
}

fn terminal_hyperlinks_enabled() -> bool {
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_OSC8").is_none()
        && std::env::var("TERM").map_or(true, |term| term != "dumb")
}

fn address_explorer_url(
    kind: ItsChainKind,
    explorer_url: Option<&str>,
    address: &str,
    role: AddressRole,
) -> Option<String> {
    let base = explorer_url?.trim();
    if base.is_empty() {
        return None;
    }

    Some(match kind {
        ItsChainKind::Evm => format!("{}/address/{address}", trim_url_path(base)),
        ItsChainKind::Solana => solana_explorer_address_url(base, address),
        ItsChainKind::Sui => {
            let segment = if role == AddressRole::Its {
                "object"
            } else {
                "address"
            };
            format!("{}/{segment}/{address}", trim_url_path(base))
        }
        ItsChainKind::Stellar => {
            let segment = if role == AddressRole::Its || address.starts_with('C') {
                "contract"
            } else {
                "account"
            };
            format!("{}/{segment}/{address}", trim_url_path(base))
        }
    })
}

fn solana_explorer_address_url(base: &str, address: &str) -> String {
    let (path, query) = base.split_once('?').unwrap_or((base, ""));
    let suffix = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    format!("{}/address/{address}{suffix}", trim_url_path(path))
}

fn trim_url_path(url: &str) -> &str {
    url.trim_end_matches('/')
}

fn short_error(error: &str) -> String {
    const LIMIT: usize = 48;
    let simplified = if error.contains("error sending request")
        || error.contains("request failed")
        || error.contains("client error")
    {
        "request failed"
    } else if error.contains("no configured candidate has operator role")
        || error.contains("no operator candidate in config")
    {
        "no operator candidate"
    } else if error.contains("returned no data") {
        "no contract data"
    } else {
        error
    };
    if simplified.chars().count() <= LIMIT {
        return simplified.to_string();
    }
    let truncated = simplified.chars().take(LIMIT).collect::<String>();
    format!("{truncated}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_address_truncates_more_aggressively() {
        assert_eq!(
            compact_address("0x1234567890abcdef1234567890abcdef12345678"),
            "0x12345678...345678"
        );
    }

    #[test]
    fn compact_address_keeps_small_values_readable() {
        assert_eq!(compact_address("0x1234"), "0x1234");
    }

    #[test]
    fn explorer_urls_use_chain_specific_paths() {
        assert_eq!(
            address_explorer_url(
                ItsChainKind::Evm,
                Some("https://sepolia.etherscan.io/"),
                "0x1234",
                AddressRole::Owner,
            )
            .as_deref(),
            Some("https://sepolia.etherscan.io/address/0x1234")
        );
        assert_eq!(
            address_explorer_url(
                ItsChainKind::Solana,
                Some("https://explorer.solana.com/?cluster=testnet"),
                "pubkey",
                AddressRole::Operator,
            )
            .as_deref(),
            Some("https://explorer.solana.com/address/pubkey?cluster=testnet")
        );
        assert_eq!(
            address_explorer_url(
                ItsChainKind::Sui,
                Some("https://suiscan.xyz/testnet"),
                "0x1234",
                AddressRole::Its,
            )
            .as_deref(),
            Some("https://suiscan.xyz/testnet/object/0x1234")
        );
    }

    #[test]
    fn terminal_link_wraps_label_without_changing_visible_text() {
        assert_eq!(
            terminal_link("0x1234...abcd", "https://example.com/address/0x1234"),
            "\x1b]8;;https://example.com/address/0x1234\x1b\\0x1234...abcd\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn overlay_terminal_links_replaces_only_visible_labels() {
        let rendered = "ITS Owner\n0x1234...abcd 0xabcd...1234";
        let links = vec![AddressLink {
            label: "0x1234...abcd".to_string(),
            url: "https://example.com/address/0x1234".to_string(),
        }];

        assert_eq!(
            overlay_terminal_links(rendered, &links),
            "ITS Owner\n\x1b]8;;https://example.com/address/0x1234\x1b\\0x1234...abcd\x1b]8;;\x1b\\ 0xabcd...1234"
        );
    }

    #[test]
    fn owner_kind_prefers_matching_governance_contract() {
        let governance = GovernanceStatus::Owner("AxelarServiceGov".to_string());

        assert_eq!(
            owner_kind(
                Some("0xabc"),
                ItsChainKind::Evm,
                &governance,
                Some(&Ok(false))
            ),
            OwnerKind::Governance("AxelarServiceGov".to_string())
        );
    }

    #[test]
    fn owner_kind_classifies_evm_code_presence() {
        let governance = GovernanceStatus::Deployed("InterchainGov".to_string());

        assert_eq!(
            owner_kind(
                Some("0xabc"),
                ItsChainKind::Evm,
                &governance,
                Some(&Ok(false))
            ),
            OwnerKind::Eoa
        );
        assert_eq!(
            owner_kind(
                Some("0xabc"),
                ItsChainKind::Evm,
                &governance,
                Some(&Ok(true))
            ),
            OwnerKind::Contract
        );
    }

    #[test]
    fn matching_governance_detects_owner_case_insensitively() {
        let governance_contracts = vec![GovernanceContract {
            name: "AxelarServiceGovernance",
            label: "service gov",
            address: "0xAbCd".to_string(),
        }];

        assert_eq!(
            matching_governance(Some("0xabcd"), &governance_contracts),
            Some("service gov".to_string())
        );
    }

    #[test]
    fn chain_kind_ignores_unsupported_chain_types() {
        assert_eq!(chain_kind(Some("xrpl")), None);
    }

    #[test]
    fn resolves_stellar_network_type_from_axelar_network() {
        let chain = ChainConfig {
            axelar_id: None,
            name: None,
            rpc: None,
            wss_rpc: None,
            network_type: None,
            chain_type: None,
            token_symbol: None,
            token_address: None,
            gas_scaling_factor: None,
            decimals: None,
            contracts: None,
            extra: Default::default(),
        };
        assert_eq!(stellar_network_type(&chain, Network::Mainnet), "mainnet");
        assert_eq!(stellar_network_type(&chain, Network::Testnet), "testnet");
    }
}
