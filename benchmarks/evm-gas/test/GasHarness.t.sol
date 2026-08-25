// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

// Gas-measurement harness for the fee-api source_gas_units calibration (OCSI-690).
//
// Measures real source-chain `gasUsed` for the bridge operations the AxelarApp
// Swap frontend does NOT exercise (so the mainnet measurement doc did not cover
// them): gmp, gmpWithToken, itsTransferWithCall, itsDeployment. Also measures a
// plain itsTransfer as a validation point against the doc's ~142k figure. Calls
// the real deployed Gateway / ITS / InterchainTokenFactory on a mainnet fork.
//
// Ethereum mainnet is one of the 15 "standard" (Ethereum-equivalent) chains, so
// a fork is accurate here; per the doc, the 4 deviating chains
// (Polygon/Monad/Moonbeam/Filecoin) must be confirmed on a real network and are
// NOT derived from a fork.
//
// MUST run with `--isolate`: that meters each top-level call as a real
// transaction (21000 intrinsic + calldata gas + execution) against cold state,
// so the measured `gasleft()` delta equals a real receipt's `gasUsed`. Without
// `--isolate` the delta is execution-only and warm-biased (validated: gmp reads
// 40,813 with --isolate vs 15,473 without; itsTransfer lands ~150k vs the doc's
// real 142k, the residual being a fresh factory token vs an established one).
//
// Run (mainnet fork; RPC defaults to a public node, override via env):
//   MAINNET_RPC_URL=<url> forge test --mc GasHarness --isolate -vv

/// Foundry cheatcodes used here (superset of test/TestBase.sol's minimal Vm).
interface Hevm {
    function createSelectFork(string calldata urlOrAlias) external returns (uint256);
    function envOr(string calldata name, string calldata defaultValue) external returns (string memory);
    function prank(address sender) external;
    function deal(address who, uint256 newBalance) external;
    function store(address target, bytes32 slot, bytes32 value) external;
    function load(address target, bytes32 slot) external view returns (bytes32);
    function mockCall(address callee, bytes calldata data, bytes calldata returnData) external;
}

interface IERC20 {
    function balanceOf(address account) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
}

interface IInterchainTokenFactory {
    function deployInterchainToken(
        bytes32 salt,
        string calldata name,
        string calldata symbol,
        uint8 decimals,
        uint256 initialSupply,
        address minter
    ) external payable returns (bytes32 tokenId);

    function deployRemoteInterchainToken(
        bytes32 salt,
        string calldata destinationChain,
        uint256 gasValue
    ) external payable returns (bytes32 tokenId);

    // The app's actual deployment path (scripts/deploy-its.ts): register an
    // existing ERC-20 as canonical (deploys a lock/unlock manager), then deploy
    // it remotely per destination.
    function registerCanonicalInterchainToken(address tokenAddress) external payable returns (bytes32 tokenId);

    function deployRemoteCanonicalInterchainToken(
        address tokenAddress,
        string calldata destinationChain,
        uint256 gasValue
    ) external payable returns (bytes32 tokenId);
}

/// Minimal but complete ERC-20 surface for ITS canonical registration (it reads
/// metadata and the token-manager setup touches the ERC-20 interface).
contract MockERC20 {
    function name() external pure returns (string memory) {
        return "Mock Token";
    }

    function symbol() external pure returns (string memory) {
        return "MCK";
    }

    function decimals() external pure returns (uint8) {
        return 18;
    }

    function totalSupply() external pure returns (uint256) {
        return 1_000_000 ether;
    }

    function balanceOf(address) external pure returns (uint256) {
        return 0;
    }

    function allowance(address, address) external pure returns (uint256) {
        return 0;
    }

    function approve(address, uint256) external pure returns (bool) {
        return true;
    }

    function transfer(address, uint256) external pure returns (bool) {
        return true;
    }

    function transferFrom(address, address, uint256) external pure returns (bool) {
        return true;
    }
}

/// The 6-arg `interchainTransfer` is the exact entrypoint the AxelarApp uses;
/// `metadata` empty = plain transfer (itsTransfer), a version-0-prefixed payload
/// = the destination-call variant (itsTransferWithCall, the app's unwrap shape).
interface IInterchainTokenService {
    function interchainTransfer(
        bytes32 tokenId,
        string calldata destinationChain,
        bytes calldata destinationAddress,
        uint256 amount,
        bytes calldata metadata,
        uint256 gasValue
    ) external payable;
}

/// ITS destination-side execute + the ITS-Hub address getter used to satisfy
/// the `onlyItsHub` guard.
interface IItsExecute {
    function execute(
        bytes32 commandId,
        string calldata sourceChain,
        string calldata sourceAddress,
        bytes calldata payload
    ) external;

    function itsHubAddress() external view returns (string memory);
}

interface IAxelarGateway {
    function callContract(
        string calldata destinationChain,
        string calldata destinationContractAddress,
        bytes calldata payload
    ) external;

    function validateContractCall(
        bytes32 commandId,
        string calldata sourceChain,
        string calldata sourceAddress,
        bytes32 payloadHash
    ) external returns (bool);

    function callContractWithToken(
        string calldata destinationChain,
        string calldata destinationContractAddress,
        bytes calldata payload,
        string calldata symbol,
        uint256 amount
    ) external;

    function tokenAddresses(string calldata symbol) external view returns (address);
}

/// Minimal ITS-executable recipient for itsTransferWithCall: ITS gives it the
/// token then calls this, which must return EXECUTE_SUCCESS. Body is empty so the
/// measurement isolates the ITS transfer+dispatch overhead; a real app contract
/// adds its own logic (e.g. the NativeUnwrapper's withdraw+send) on top.
contract MockExecutable {
    function executeWithInterchainToken(
        bytes32,
        string calldata,
        bytes calldata,
        bytes calldata,
        bytes32,
        address,
        uint256
    ) external pure returns (bytes32) {
        return keccak256("its-execute-success");
    }
}

contract GasHarnessTest {
    // DSTest-signature events; `forge test -vv` prints these.
    event log_named_uint(string key, uint256 val);
    event log_named_string(string key, string val);

    Hevm internal constant vm = Hevm(address(uint160(uint256(keccak256("hevm cheat code")))));

    // Ethereum mainnet (a "standard" Ethereum-equivalent chain; fork-accurate).
    address internal constant ETH_GATEWAY = 0x4F4495243837681061C4743b74B3eEdf548D56A5;
    // ITS + InterchainTokenFactory — same CREATE3 address on every EVM chain.
    address internal constant ITS = 0xB5FB4BE02232B1bBA4dC8f81dc24C26980dE9e3C;
    address internal constant ITS_FACTORY = 0x83a93500d23Fbc3e82B410aD07A6a9F7A0670D66;
    string internal constant DEFAULT_MAINNET_RPC = "https://ethereum-rpc.publicnode.com";
    // The ITS-trusted destination is the capitalized axelarId, not "avalanche".
    string internal constant DEST_CHAIN = "Avalanche";
    // ITS Hub (CosmWasm on the Axelar chain) — the required source for inbound
    // execute (onlyItsHub). From axelar-contract-deployments mainnet config.
    string internal constant ITS_HUB_CHAIN = "axelar";
    string internal constant ITS_HUB_ADDRESS =
        "axelar1aqcj54lzz0rk22gvqgcn8fr5tx4rzwdv5wv5j9dmnacgefvd7wzsy2j2mr";

    address internal caller = address(0xA11CE);

    function _forkMainnet() internal {
        vm.createSelectFork(vm.envOr("MAINNET_RPC_URL", DEFAULT_MAINNET_RPC));
        vm.deal(caller, 100 ether);
    }

    /// Seed an ERC-20 balance by brute-forcing the `balanceOf` mapping slot
    /// (stdstore-style), so no whale prank is needed. Reverts if no slot in
    /// 0..30 controls the balance.
    function _dealToken(address token, address to, uint256 amount) internal {
        for (uint256 slot = 0; slot < 30; slot++) {
            bytes32 key = keccak256(abi.encode(to, slot));
            bytes32 prev = vm.load(token, key);
            vm.store(token, key, bytes32(amount));
            if (IERC20(token).balanceOf(to) == amount) return;
            vm.store(token, key, prev);
        }
        revert("balanceOf slot not found");
    }

    /// Deploy a fresh mint/burn (NATIVE_INTERCHAIN) interchain token via the
    /// factory with supply minted to `caller`, so transfers burn from the caller
    /// with no approve and no whale-hunting. Returns its tokenId.
    function _deployTokenWithSupply(bytes32 salt) internal returns (bytes32 tokenId) {
        vm.prank(caller);
        tokenId = IInterchainTokenFactory(ITS_FACTORY).deployInterchainToken(
            salt, "Harness Xfer", "HXF", 18, 1_000_000 ether, caller
        );
    }

    function _emit(string memory op, string memory metric, uint256 gasUsed) internal {
        emit log_named_string("op", op);
        emit log_named_uint(metric, gasUsed);
    }

    /// gmp: generic contract call, no token. Source tx = `Gateway.callContract`.
    /// Payload sized to the config's execution `calldata_bytes` (150 bytes).
    function test_gmp_source_gas() public {
        _forkMainnet();
        bytes memory payload = new bytes(150);
        for (uint256 i = 0; i < payload.length; i++) {
            payload[i] = 0xab;
        }
        vm.prank(caller);
        uint256 g = gasleft();
        IAxelarGateway(ETH_GATEWAY).callContract(DEST_CHAIN, "0x1234", payload);
        _emit("gmp (source, ethereum)", "gmp gasUsed", g - gasleft());
    }

    /// gmpWithToken: `approve(gateway)` (one-time) + `callContractWithToken` with
    /// a gateway-registered token (axlUSDC). Both measured.
    function test_gmpWithToken_source_gas() public {
        _forkMainnet();
        // ETH's gateway registers native tokens directly (lock/unlock); USDC is
        // the representative gateway token here.
        string memory symbol = "USDC";
        address token = IAxelarGateway(ETH_GATEWAY).tokenAddresses(symbol);
        require(token != address(0), "gateway token not registered");
        uint256 amount = 1_000_000; // 1 USDC (6 decimals)
        _dealToken(token, caller, amount);

        vm.prank(caller);
        uint256 gA = gasleft();
        IERC20(token).approve(ETH_GATEWAY, amount);
        uint256 approveGas = gA - gasleft();

        vm.prank(caller);
        uint256 gC = gasleft();
        IAxelarGateway(ETH_GATEWAY).callContractWithToken(DEST_CHAIN, "0x1234", hex"ab", symbol, amount);
        uint256 callGas = gC - gasleft();

        emit log_named_string("op", "gmpWithToken (source, ethereum)");
        emit log_named_uint("gmpWithToken approve gasUsed (one-time)", approveGas);
        emit log_named_uint("gmpWithToken callContractWithToken gasUsed", callGas);
        emit log_named_uint("gmpWithToken first-time total gasUsed", approveGas + callGas);
    }

    /// itsTransfer: plain interchain transfer (empty metadata). Validation point
    /// against the measurement doc's ~142k mint/burn figure.
    function test_itsTransfer_source_gas() public {
        _forkMainnet();
        bytes32 tokenId = _deployTokenWithSupply(keccak256("harness-xfer"));
        vm.prank(caller);
        uint256 g = gasleft();
        IInterchainTokenService(ITS).interchainTransfer{value: 0.01 ether}(
            tokenId, DEST_CHAIN, abi.encodePacked(address(0xBEEF)), 1 ether, "", 0.01 ether
        );
        _emit("itsTransfer (source, ethereum, mint/burn) [validation vs ~142k]", "itsTransfer gasUsed", g - gasleft());
    }

    /// itsTransferWithCall: transfer + destination call, via the app's version-0
    /// metadata (4-byte version + abi-encoded recipient), the app's unwrap shape.
    function test_itsTransferWithCall_source_gas() public {
        _forkMainnet();
        bytes32 tokenId = _deployTokenWithSupply(keccak256("harness-xfer-call"));
        bytes memory metadata = abi.encodePacked(uint32(0), abi.encode(address(0xBEEF)));
        vm.prank(caller);
        uint256 g = gasleft();
        IInterchainTokenService(ITS).interchainTransfer{value: 0.01 ether}(
            tokenId, DEST_CHAIN, abi.encodePacked(address(0xBEEF)), 1 ether, metadata, 0.01 ether
        );
        _emit("itsTransferWithCall (source, ethereum)", "itsTransferWithCall gasUsed", g - gasleft());
    }

    /// itsDeployment SOURCE — the app's actual canonical-registration flow
    /// (scripts/deploy-its.ts): register an existing ERC-20 as canonical (deploys
    /// a lock/unlock manager, one-time) then deploy it remotely per destination.
    /// This is far lighter than deploying a brand-new interchain token, and is
    /// what axelar-app generates. (For contrast, a new-token `deployInterchainToken`
    /// source runs ~842k; not the app's path.)
    function test_itsDeployment_source_gas() public {
        _forkMainnet();
        IInterchainTokenFactory factory = IInterchainTokenFactory(ITS_FACTORY);
        address token = address(new MockERC20());

        vm.prank(caller);
        uint256 g0 = gasleft();
        factory.registerCanonicalInterchainToken(token);
        uint256 registerGas = g0 - gasleft();

        vm.prank(caller);
        uint256 g1 = gasleft();
        factory.deployRemoteCanonicalInterchainToken{value: 0.01 ether}(token, DEST_CHAIN, 0.01 ether);
        uint256 remoteGas = g1 - gasleft();

        emit log_named_string("op", "itsDeployment (source, ethereum, canonical - app flow)");
        emit log_named_uint("itsDeployment register gasUsed (one-time)", registerGas);
        emit log_named_uint("itsDeployment remote-leg gasUsed (per dest)", remoteGas);
        emit log_named_uint("itsDeployment register+1remote gasUsed", registerGas + remoteGas);
    }

    /// One inbound ITS execute of a transfer to `recipient`. The gateway's
    /// message validation is mocked to true (a real approval needs a verifier
    /// proof); `onlyItsHub` is satisfied with the real hub chain/address, and the
    /// payload is the Hub-wrapped (RECEIVE_FROM_HUB) interchain-transfer shape.
    /// Run one inbound ITS `execute` for a Hub-wrapped `inner` ITS message and
    /// return its gasUsed. Gateway validation is mocked true; `onlyItsHub` is
    /// satisfied with the real hub chain/address.
    function _executeInbound(bytes memory inner, bytes32 commandId) internal returns (uint256 gasUsed) {
        bytes memory hubPayload = abi.encode(uint256(4), "Avalanche", inner); // RECEIVE_FROM_HUB
        vm.mockCall(
            ETH_GATEWAY,
            abi.encodeWithSelector(IAxelarGateway.validateContractCall.selector),
            abi.encode(true)
        );
        uint256 g = gasleft();
        IItsExecute(ITS).execute(commandId, ITS_HUB_CHAIN, ITS_HUB_ADDRESS, hubPayload);
        gasUsed = g - gasleft();
    }

    function _executeTransfer(bytes32 tokenId, address recipient, bytes32 commandId)
        internal
        returns (uint256 gasUsed)
    {
        bytes memory inner = abi.encode(
            uint256(0), // MESSAGE_TYPE_INTERCHAIN_TRANSFER
            tokenId,
            abi.encodePacked(caller), // sourceAddress bytes
            abi.encodePacked(recipient), // destinationAddress bytes (20B -> toAddress)
            uint256(1 ether),
            bytes("") // no data => plain transfer
        );
        return _executeInbound(inner, commandId);
    }

    /// itsTransfer DESTINATION execute: mint/unlock to the recipient. Measured
    /// fresh (recipient's first receipt, cold 0->nonzero) and existing.
    ///
    /// Add ~8k to each figure for the real gateway's message-validation SSTORE
    /// that `vm.mockCall` skips (marking the command executed) — a real `execute`
    /// pays it. With that offset these reproduce the doc's §9 ~107k existing /
    /// ~124k fresh; the +17k fresh delta is measured directly and needs no offset.
    function test_dst_execute_itsTransfer_gas() public {
        _forkMainnet();
        bytes32 tokenId = _deployTokenWithSupply(keccak256("harness-dst-exec"));
        address fresh = address(0xF00D);

        uint256 freshGas = _executeTransfer(tokenId, fresh, keccak256("cmd-fresh"));
        uint256 existingGas = _executeTransfer(tokenId, fresh, keccak256("cmd-existing"));

        emit log_named_string("op", "itsTransfer (DESTINATION execute, ethereum)");
        emit log_named_uint("dst execute fresh recipient gasUsed", freshGas);
        emit log_named_uint("dst execute existing recipient gasUsed", existingGas);
    }

    /// itsTransferWithCall DESTINATION execute: ITS mints to the executable
    /// recipient contract, then calls executeWithInterchainToken. Isolates the
    /// ITS transfer+dispatch overhead (empty executable body); a real app adds
    /// its own logic (cf. the doc's unwrap contract-call ~150k, which includes
    /// the NativeUnwrapper's withdraw+send). +~8k for the mocked gateway SSTORE.
    function test_dst_execute_itsTransferWithCall_gas() public {
        _forkMainnet();
        bytes32 tokenId = _deployTokenWithSupply(keccak256("harness-dst-exec-call"));
        MockExecutable recipient = new MockExecutable();

        bytes memory inner = abi.encode(
            uint256(0), // INTERCHAIN_TRANSFER
            tokenId,
            abi.encodePacked(caller),
            abi.encodePacked(address(recipient)),
            uint256(1 ether),
            abi.encode("payload") // non-empty data => executeWithInterchainToken
        );
        uint256 gasUsed = _executeInbound(inner, keccak256("cmd-xfer-call"));
        _emit("itsTransferWithCall (DESTINATION execute, ethereum)", "dst execute gasUsed (+~8k for mock)", gasUsed);
    }

    /// itsDeployment DESTINATION execute: inbound DEPLOY_INTERCHAIN_TOKEN deploys
    /// the token + NATIVE_INTERCHAIN manager on the destination. The heaviest
    /// destination execute (contract creation). +~8k for the mocked gateway SSTORE.
    function test_dst_execute_itsDeployment_gas() public {
        _forkMainnet();
        bytes32 tokenId = keccak256("harness-dst-deploy-id"); // fresh, undeployed
        bytes memory inner = abi.encode(
            uint256(1), // DEPLOY_INTERCHAIN_TOKEN
            tokenId,
            "Dst Token",
            "DST",
            uint8(18),
            bytes("") // no minter
        );
        uint256 gasUsed = _executeInbound(inner, keccak256("cmd-deploy"));
        _emit("itsDeployment (DESTINATION execute, ethereum)", "dst execute gasUsed (+~8k for mock)", gasUsed);
    }
}
