//! The MCP server: its tool set, its resources, and the protocol handshake.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Implementation, ListResourcesResult, PaginatedRequestParams, ProtocolVersion,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, ResourceContents,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::cli::{EvmContract, SolProgram};
use crate::commands::load_test::{self, Protocol, TestType};
use crate::commands::{
    check_balances, decode, decode_evm_activity, decode_sol_activity, decode_tx, info_block,
    its_ownership, test_express, verifier_votes, verifiers,
};
use crate::mcp::context::McpContext;
use crate::mcp::guidance;
use crate::mcp::outcome::{Outcome, to_error_data};
use crate::mcp::runs::{RunRegistry, RunState};

/// Arguments for the block lookup.
///
/// Deliberately carries no network: the server was started against one
/// network and a tool cannot move it. See [`McpContext`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BlockArgs {
    /// Block height. Omit for the current head. A height above the head is
    /// predicted from the recent block rate.
    pub number: Option<u64>,
    /// Predict the block at this time, as RFC3339 or unix seconds. Cannot be
    /// combined with a height.
    pub at_time: Option<String>,
}

/// Arguments for the route check.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RouteArgs {
    /// gmp for callContract, its for interchainTransfer, or its-with-data.
    pub protocol: Protocol,
    /// The chain-type pairing, for example sol-to-evm or evm-to-xrpl.
    pub route: TestType,
    /// Source chain axelar id, for example solana.
    pub source_chain: String,
    /// Destination chain axelar id, for example flow.
    pub destination_chain: String,
}

/// How many recent entries to report when the caller does not say.
const DEFAULT_ACTIVITY_LIMIT: usize = 20;

/// Arguments for the Solana activity scan.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SolActivityArgs {
    /// Restrict to one program: gateway, its, gas-service or memo. Omit for all.
    pub program: Option<SolProgram>,
    /// Recent transactions per program. Defaults to 20.
    pub limit: Option<usize>,
}

impl SolActivityArgs {
    fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_ACTIVITY_LIMIT)
    }
}

/// Arguments for the EVM activity scan.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvmActivityArgs {
    /// Chain axelar id, for example avalanche-fuji.
    pub chain: String,
    /// Restrict to one contract: gateway, its or gas-service. Omit for all.
    pub contract: Option<EvmContract>,
    /// Recent events per contract. Defaults to 20.
    pub limit: Option<usize>,
}

impl EvmActivityArgs {
    fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_ACTIVITY_LIMIT)
    }
}

/// Arguments for the calldata decoder.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalldataArgs {
    /// Hex calldata, with or without a leading 0x.
    pub calldata: String,
}

/// Arguments for the transaction decoder.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TxArgs {
    /// EVM transaction hash, starting with 0x.
    pub tx_hash: String,
    /// Restrict the search to one chain axelar id. Omit to search all
    /// configured EVM chains.
    pub chain: Option<String>,
}

/// Arguments for the express transfer scan.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExpressScanArgs {
    /// Express-supported chain axelar ids to scan.
    pub chains: Vec<String>,
    /// Recent transfers per chain. Defaults to 5.
    pub recent: Option<usize>,
}

impl ExpressScanArgs {
    fn recent(&self) -> usize {
        self.recent.unwrap_or(0)
    }
}

/// Arguments for starting a load test.
///
/// Carries no keys, no RPC overrides and no config path: those come from the
/// operator environment the server was launched with. Nothing an agent sends
/// can substitute a different signer.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StartLoadTestArgs {
    /// Source chain axelar id, for example solana.
    pub source_chain: String,
    /// Destination chain axelar id, for example flow.
    pub destination_chain: String,
    /// gmp for callContract, its for interchainTransfer, or its-with-data.
    pub protocol: Option<Protocol>,
    /// The chain-type pairing. Omit to let axe infer it from the config.
    pub route: Option<TestType>,
    /// How many transactions to send. Defaults to 1.
    pub num_txs: Option<u64>,
}

/// Arguments for a tool that names one background run.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunArgs {
    /// The run identifier returned by start_load_test.
    pub run_id: String,
}

/// Arguments for a tool that names one chain.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChainArgs {
    /// Chain axelar id, for example solana or avalanche-fuji.
    pub chain: String,
}

/// Arguments for the verifier vote lookup.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct VerifierVotesArgs {
    /// Chain axelar id whose polls to inspect.
    pub chain: String,
    /// The verifier axelar1... address.
    pub verifier: String,
    /// Most recent votes to report. Defaults to 20.
    pub limit: Option<usize>,
}

impl VerifierVotesArgs {
    fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_ACTIVITY_LIMIT)
    }
}

/// Serves axe's commands as MCP tools over a single pinned network.
#[derive(Clone)]
pub struct AxeMcp {
    context: McpContext,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl AxeMcp {
    pub fn new(context: McpContext) -> Self {
        Self {
            context,
            tool_router: Self::tool_router(),
        }
    }

    /// Look up an Axelar block height and its timestamp. With no arguments
    /// this reports the current head. Reach for this to place an event in
    /// time, or to predict when a future height will be reached.
    #[tool(name = "info_block")]
    pub async fn info_block(
        &self,
        Parameters(args): Parameters<BlockArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if args.number.is_some() && args.at_time.is_some() {
            return Err(ErrorData::invalid_params(
                "pass either a height or a time, not both",
                None,
            ));
        }

        let network = self.context.network();
        let info = info_block::resolve(network, args.number, args.at_time)
            .await
            .map_err(|e| to_error_data("block lookup failed", &e))?;

        let verb = if info.predicted { "predicted at" } else { "at" };
        let summary = format!(
            "block {} on {network} {verb} {}",
            info.height,
            info.time.format("%Y-%m-%d %H:%M:%S UTC")
        );

        Outcome::new(summary, &info)
            .map(Outcome::into_tool_result)
            .map_err(|e| to_error_data("could not serialize block info", &e))
    }

    /// Check whether a cross-chain route can be attempted, before spending
    /// anything on it. Reach for this first: an unsupported pairing fails
    /// partway through a flow, after funds have already moved.
    #[tool(name = "check_route")]
    pub async fn check_route(
        &self,
        Parameters(args): Parameters<RouteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let support = guidance::check_route(
            args.protocol,
            args.route,
            &args.source_chain,
            &args.destination_chain,
        );

        let verdict = if support.supported {
            "is supported"
        } else {
            "is NOT supported"
        };
        let summary = format!(
            "{} {} -> {} {verdict}",
            support.protocol, support.source_chain, support.destination_chain
        );

        Outcome::new(summary, &support)
            .map(Outcome::into_tool_result)
            .map_err(|e| to_error_data("could not serialize route support", &e))
    }

    /// Recent on-chain activity of the Axelar Solana programs, decoded into
    /// named instructions and events. Reach for this to see what a program has
    /// actually been doing, or to confirm a message landed on Solana.
    #[tool(name = "decode_sol_activity")]
    pub async fn decode_sol_activity(
        &self,
        Parameters(args): Parameters<SolActivityArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let network = self.context.network();
        let entries = decode_sol_activity::resolve(args.program, Some(network), args.limit())
            .await
            .map_err(|e| to_error_data("solana activity scan failed", &e))?;

        let summary = format!("{} recent Solana entries on {network}", entries.len());

        Outcome::new(summary, &entries)
            .map(Outcome::into_tool_result)
            .map_err(|e| to_error_data("could not serialize solana activity", &e))
    }

    /// Recent events emitted by the Axelar EVM contracts on one chain, decoded
    /// into named events with typed parameters. Reach for this to correlate a
    /// source-chain event with its destination-chain execution.
    #[tool(name = "decode_evm_activity")]
    pub async fn decode_evm_activity(
        &self,
        Parameters(args): Parameters<EvmActivityArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let network = self.context.network();
        let entries =
            decode_evm_activity::resolve(args.contract, network, args.chain.clone(), args.limit())
                .await
                .map_err(|e| to_error_data("evm activity scan failed", &e))?;

        let summary = format!(
            "{} recent events on {} ({network})",
            entries.len(),
            args.chain
        );

        Outcome::new(summary, &entries)
            .map(Outcome::into_tool_result)
            .map_err(|e| to_error_data("could not serialize evm activity", &e))
    }

    /// The verifiers currently attesting to a chain, with weights and
    /// registration state. Reach for this to answer who is securing a chain,
    /// or to see whether a verifier set has formed yet.
    #[tool(name = "verifiers")]
    pub async fn verifiers(
        &self,
        Parameters(args): Parameters<ChainArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let network = self.context.network();
        let report = verifiers::resolve(network, &args.chain)
            .await
            .map_err(|e| to_error_data("verifier lookup failed", &e))?;

        let active = report
            .get("verifiers")
            .and_then(|v| v.as_array())
            .map_or(0, Vec::len);
        let summary = format!("{active} verifiers listed for {} on {network}", args.chain);

        Outcome::new(summary, &report)
            .map(Outcome::into_tool_result)
            .map_err(|e| to_error_data("could not serialize verifiers", &e))
    }

    /// Recent votes cast by one verifier on one chain. Reach for this when a
    /// message failed verification and you need to see whether a specific
    /// verifier voted against it or missed the poll.
    #[tool(name = "verifier_votes")]
    pub async fn verifier_votes(
        &self,
        Parameters(args): Parameters<VerifierVotesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let network = self.context.network();
        let report = verifier_votes::resolve(network, &args.chain, &args.verifier, args.limit())
            .await
            .map_err(|e| to_error_data("verifier vote lookup failed", &e))?;

        let votes = report
            .get("votes")
            .and_then(|v| v.as_array())
            .map_or(0, Vec::len);
        let summary = format!(
            "{votes} recent votes by {} on {} ({network})",
            args.verifier, args.chain
        );

        Outcome::new(summary, &report)
            .map(Outcome::into_tool_result)
            .map_err(|e| to_error_data("could not serialize verifier votes", &e))
    }

    /// Who owns and operates the ITS deployment on every chain in a network.
    /// Reach for this to audit control of the token layer, or to check whether
    /// governance holds ownership where it should.
    #[tool(name = "its_ownership")]
    pub async fn its_ownership(&self) -> Result<CallToolResult, ErrorData> {
        let network = self.context.network();
        let report = its_ownership::resolve(network)
            .await
            .map_err(|e| to_error_data("ITS ownership lookup failed", &e))?;

        let rows = report
            .pointer("/summary/rows")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let summary = format!("ITS ownership for {rows} chains on {network}");

        Outcome::new(summary, &report)
            .map(Outcome::into_tool_result)
            .map_err(|e| to_error_data("could not serialize ITS ownership", &e))
    }

    /// Whether the load-test wallets hold enough native gas and AXE for a run.
    /// Reach for this before starting any flow that spends funds: it reports
    /// which wallet is short rather than just passing or failing.
    #[tool(name = "check_balances")]
    pub async fn check_balances(&self) -> Result<CallToolResult, ErrorData> {
        let network = self.context.network();
        let report = check_balances::resolve(network)
            .await
            .map_err(|e| to_error_data("balance check failed", &e))?;

        let short = report
            .pointer("/summary/underfunded")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let summary = if short == 0 {
            format!("all wallets funded on {network}")
        } else {
            format!("{short} wallet(s) underfunded on {network}")
        };

        Outcome::new(summary, &report)
            .map(Outcome::into_tool_result)
            .map_err(|e| to_error_data("could not serialize balance check", &e))
    }

    /// Decode EVM calldata into a function signature and named, typed
    /// arguments, using axe's embedded ABI database. Reach for this when you
    /// have a hex payload and need to know what it represents.
    ///
    /// Also recognises ITS messages including hub frames, governance proposal
    /// payloads, and printable text. A few rarer fallback shapes are still
    /// CLI-only, and come back as unrecognised: run axe decode for those.
    #[tool(name = "decode_calldata")]
    pub async fn decode_calldata(
        &self,
        Parameters(args): Parameters<CalldataArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let decoded = decode::decode_payload_hex(&args.calldata)
            .map_err(|e| to_error_data("calldata decode failed", &e))?;

        // The summary names the shape, because that is the first thing a
        // caller needs in order to know what the fields mean.
        let summary = match &decoded {
            decode::DecodedPayload::FunctionCall(call) => {
                format!(
                    "{} with {} argument(s)",
                    call.signature,
                    call.arguments.len()
                )
            }
            decode::DecodedPayload::ItsMessage { name, fields } => {
                format!("ITS {name} with {} field(s)", fields.len())
            }
            decode::DecodedPayload::GovernanceProposal {
                command_name,
                target,
                ..
            } => format!("governance {command_name} targeting {target}"),
            decode::DecodedPayload::Text { .. } => "printable text".to_string(),
            // Not an error: the CLI has further fallback patterns that are
            // still printer-only, so it may say more about these bytes.
            decode::DecodedPayload::Unrecognised { .. } => {
                "not a recognised payload shape".to_string()
            }
        };

        Outcome::new(summary, &decoded)
            .map(Outcome::into_tool_result)
            .map_err(|e| to_error_data("could not serialize decoded payload", &e))
    }

    /// Fetch and decode an EVM transaction: which chain it landed on, its
    /// status, its decoded input, and its decoded events. Reach for this when
    /// you have a transaction hash and need to know what it did.
    ///
    /// EVM only. Solana signatures are not decoded here; run axe decode tx for
    /// those.
    #[tool(name = "decode_tx")]
    pub async fn decode_tx(
        &self,
        Parameters(args): Parameters<TxArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if !args.tx_hash.starts_with("0x") {
            return Err(ErrorData::invalid_params(
                "this tool decodes EVM transaction hashes, which start with 0x; \
                 run axe decode tx for a Solana signature",
                None,
            ));
        }

        let decoded = decode_tx::resolve_evm(&args.tx_hash, None, args.chain.as_deref())
            .await
            .map_err(|e| to_error_data("transaction decode failed", &e))?;

        let status = match decoded.succeeded {
            Some(true) => "succeeded",
            Some(false) => "failed",
            None => "status unknown",
        };
        let summary = format!(
            "{} on {}, {status}, {} event(s)",
            decoded.tx_hash,
            decoded.chain,
            decoded.logs.len()
        );

        Outcome::new(summary, &decoded)
            .map(Outcome::into_tool_result)
            .map_err(|e| to_error_data("could not serialize decoded transaction", &e))
    }

    /// Recent express transfers on one or more chains, with each transfer's
    /// two phases: whether an express executor fronted the funds, and whether
    /// the canonical execute landed to reimburse it. Observe-only, spends
    /// nothing. Reach for this to investigate express reimbursement.
    #[tool(name = "express_scan")]
    pub async fn express_scan(
        &self,
        Parameters(args): Parameters<ExpressScanArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let network = self.context.network();
        let transfers = test_express::resolve_scan(network, &args.chains, args.recent())
            .await
            .map_err(|e| to_error_data("express scan failed", &e))?;

        let reimbursed = transfers
            .iter()
            .filter(|t| t.phase2 == "reimbursed")
            .count();
        let summary = format!(
            "{} express transfer(s) on {network}, {reimbursed} reimbursed",
            transfers.len()
        );

        Outcome::new(summary, &transfers)
            .map(Outcome::into_tool_result)
            .map_err(|e| to_error_data("could not serialize express scan", &e))
    }

    /// Start a cross-chain load test in the background and return its run
    /// identifier. Reach for this to exercise a route end to end.
    ///
    /// This spends real funds on the pinned network. It returns immediately
    /// rather than waiting, because a run can outlast a request timeout, and a
    /// cancelled request would lose the record of what was already spent. Poll
    /// load_test_report with the identifier to get the result. Check the route
    /// first, and check balances, so a run is not started that cannot finish.
    #[tool(name = "start_load_test")]
    pub async fn start_load_test(
        &self,
        Parameters(args): Parameters<StartLoadTestArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let network = self.context.network();
        let run_id = RunRegistry::new_run_id();

        let flow_args = self
            .build_load_test_args(&args, network, run_id.clone())
            .await
            .map_err(|e| to_error_data("could not prepare the load test", &e))?;

        self.context
            .runs()
            .spawn_blocking_flow(&run_id, move || async move {
                // The report artifact records the outcome, including failure, so
                // nothing is lost by not observing the result here.
                let _ = load_test::run(flow_args).await;
            });

        let started = serde_json::json!({
            "run_id": run_id,
            "network": network.to_string(),
            "source_chain": args.source_chain,
            "destination_chain": args.destination_chain,
            "transactions": args.num_txs.unwrap_or(1),
        });

        Outcome::new(
            format!(
                "started {run_id}: {} -> {} on {network}",
                args.source_chain, args.destination_chain
            ),
            &started,
        )
        .map(Outcome::into_tool_result)
        .map_err(|e| to_error_data("could not serialize run start", &e))
    }

    /// Read the report of a load test by its run identifier. A run still in
    /// progress reports as running; one with no report reports as unknown,
    /// which is not the same thing.
    #[tool(name = "load_test_report")]
    pub async fn load_test_report(
        &self,
        Parameters(args): Parameters<RunArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let state = self.context.runs().state(&args.run_id).await;

        let summary = match &state {
            RunState::Running { run_id } => format!("{run_id} is still running"),
            RunState::Finished { run_id, .. } => format!("{run_id} finished, report attached"),
            RunState::Unknown { run_id } => {
                format!("{run_id} has no report and is not running here")
            }
        };

        Outcome::new(summary, &state)
            .map(Outcome::into_tool_result)
            .map_err(|e| to_error_data("could not serialize run state", &e))
    }

    /// List known load-test runs, newest first. Reach for this when a run
    /// identifier has been lost, or to see what has been run recently.
    #[tool(name = "list_load_test_runs")]
    pub async fn list_load_test_runs(&self) -> Result<CallToolResult, ErrorData> {
        let runs = self.context.runs().list().await;
        let summary = format!("{} known load-test run(s)", runs.len());

        Outcome::new(summary, &runs)
            .map(Outcome::into_tool_result)
            .map_err(|e| to_error_data("could not serialize run list", &e))
    }
}

impl AxeMcp {
    /// Build the flow arguments from the narrow tool arguments.
    ///
    /// Everything the tool does not expose is resolved here: the chains config
    /// from the pinned network, and the signing keys from the environment the
    /// operator launched the server with.
    async fn build_load_test_args(
        &self,
        args: &StartLoadTestArgs,
        network: crate::types::Network,
        run_id: String,
    ) -> eyre::Result<load_test::LoadTestArgs> {
        let config = crate::config_source::resolve(network, None)
            .await?
            .into_path();

        let resolved = load_test::resolve_from_config(
            &config,
            args.route,
            Some(args.source_chain.clone()),
            Some(args.destination_chain.clone()),
            std::env::var("EVM_PRIVATE_KEY").ok(),
            None,
            None,
        )
        .await?;

        Ok(load_test::LoadTestArgs {
            config,
            network,
            test_type: resolved.test_type,
            protocol: args.protocol.unwrap_or_default(),
            destination_chain: resolved.destination_chain,
            source_chain: resolved.source_chain,
            source_axelar_id: resolved.source_axelar_id,
            destination_axelar_id: resolved.destination_axelar_id,
            source_rpc: resolved.source_rpc,
            destination_rpc: resolved.destination_rpc,
            private_key: resolved.private_key,
            num_txs: args.num_txs.unwrap_or(1),
            keypair: std::env::var("SOLANA_PRIVATE_KEY").ok(),
            payload: None,
            gas_value: None,
            token_id: None,
            coin_type: None,
            tps: None,
            duration_secs: None,
            key_cycle: 1,
            extra_accounts: 0,
            run_id: Some(run_id),
        })
    }
}

// `router = self.tool_router` uses the router built once in `new`. Left to
// default, the macro calls `Self::tool_router()` on every request and rebuilds
// the whole tool set each time.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for AxeMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::LATEST)
        .with_server_info(Implementation::from_build_env())
        .with_instructions(format!(
            "axe drives Axelar cross-chain development. This server is pinned to \
             the {} network and no tool can change it. Private keys and RPC \
             overrides come from the operator's environment, never from tool \
             arguments. Check a route before starting any flow that spends funds, \
             and read the documentation resources for how a flow behaves.",
            self.context.network()
        ))
    }

    async fn list_resources(
        &self,
        _params: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(
            guidance::doc_resources(),
        ))
    }

    async fn read_resource(
        &self,
        params: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let body = guidance::doc_body(&params.uri).ok_or_else(|| {
            ErrorData::invalid_params(
                format!("no such documentation resource: {}", params.uri),
                None,
            )
        })?;

        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            vec![ResourceContents::text(body, params.uri)],
        )))
    }
}
