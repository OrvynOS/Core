//! Orvyn RNG on chain, end to end on a local chain (anvil, Prague, chain id 56) with the real
//! contracts (contracts/out, from `forge build`) and the live drand beacon:
//! - nodes register with proofs made by orvyn-core (rng::chain::registration_proof);
//! - a consumer (DiceRoller) on a funded subscription asks for randomness;
//! - the relayer code commits the round inside its window and fulfils it once drand is out;
//! - the contract's output equals the off-chain record's, the dice gets its roll, and the round's
//!   agents are credited their share;
//! - a price round (observe, report, sign, finalise) is relayed to OrvynPriceFeed, accepted with
//!   the staked nodes' signatures, and read back through the Chainlink-style adapter.
//!
//! Needs Foundry: `forge build` in contracts/, anvil on PATH (or ANVIL=path), then
//! `cargo test -p orvyn-core --test rng_chain -- --ignored --nocapture`.
mod support;

use alloy::network::TransactionBuilder;
use alloy::primitives::{utils::parse_ether, Address, Bytes, FixedBytes, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolConstructor;
use orvyn_core::rng::{self, bls, chain};
use orvyn_core::{price, secrets};
use serde_json::{json, Value};

sol! {
    #[sol(rpc)]
    contract OrvynRNG {
        constructor(address owner_, address committer_, address treasury_, uint16 minParticipants_, uint256 baseFee_, uint256 callbackFeePerGas_, uint16 nodesBps_, uint16 fulfillerBps_);
        function register(bytes publicKey, address payee, bytes proof) payable returns (uint256);
        function createSubscription() returns (uint256);
        function fund(uint256 subId) payable;
        function setConsumer(uint256 subId, address consumer, bool allowed);
        function randomness(uint64 round) view returns (bytes32);
        function earnings(address account) view returns (uint256);
        function nodeCount() view returns (uint256);
    }
    #[sol(rpc)]
    contract DiceRoller {
        constructor(address rng, uint256 subId);
        function roll() returns (uint256);
        function lastRoll(address player) view returns (uint8);
    }
    #[sol(rpc)]
    contract OrvynPriceFeed {
        constructor(address owner_, address network_, address treasury_);
        function configureFeed(string pair, uint16 minSigners, uint16 deviationBps, uint32 heartbeat, address gate, uint16 gateBps, uint256 reportFee, uint16 nodesBps) returns (bytes32);
        function fund(bytes32 feedId) payable;
        function latest(bytes32 feedId) view returns (int256, uint64, uint64, uint16);
        function earnings(address account) view returns (uint256);
    }
    #[sol(rpc)]
    contract OrvynFeedAdapter {
        constructor(address hub_, bytes32 feedId_);
        function latestRoundData() view returns (uint80, int256, uint256, uint256, uint80);
        function description() view returns (string);
    }
}

// anvil's default funded accounts.
const KEYS: [&str; 4] = [
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
    "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
    "0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6",
];

fn bytecode(file: &str, name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../contracts/out/{file}/{name}.json"));
    let artifact: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("build the contracts first (forge build): {}", path.display()))).unwrap();
    hex::decode(artifact["bytecode"]["object"].as_str().unwrap().trim_start_matches("0x")).unwrap()
}

struct Anvil(std::process::Child);

impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wallet(rpc: &str, key: &str) -> DynProvider {
    ProviderBuilder::new().wallet(key.parse::<PrivateKeySigner>().unwrap()).connect_http(rpc.parse().unwrap()).erased()
}

async fn deploy(p: &DynProvider, code: Vec<u8>, args: Vec<u8>) -> Address {
    let tx = TransactionRequest::default().with_deploy_code([code, args].concat());
    p.send_transaction(tx).await.unwrap().get_receipt().await.unwrap().contract_address.expect("deployed")
}

async fn set_time(p: &DynProvider, t: i64) {
    let _: Value = p.raw_request("evm_setNextBlockTimestamp".into(), [t]).await.unwrap();
    let _: Value = p.raw_request("evm_mine".into(), ()).await.unwrap();
}

#[tokio::test]
#[ignore = "needs Foundry (anvil, and contracts built with forge build)"]
async fn rng_and_price_feeds_on_chain() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let rpc = format!("http://127.0.0.1:{port}");
    // A past round whose drand round (after the commit window) is already published.
    let round = rng::round_of(rng::now_s()) - 10;
    let start = rng::round_start(round);
    let _anvil = Anvil(
        std::process::Command::new(std::env::var("ANVIL").unwrap_or_else(|_| "anvil".into()))
            .args(["--port", &port.to_string(), "--chain-id", "56", "--hardfork", "prague", "--timestamp", &(start - 40).to_string(), "--silent"])
            .spawn()
            .expect("anvil (install Foundry, or set ANVIL)"),
    );
    let accounts: Vec<Address> = KEYS.iter().map(|k| k.parse::<PrivateKeySigner>().unwrap().address()).collect();
    let (deployer, relayer, node_a, node_b) = (accounts[0], accounts[1], accounts[2], accounts[3]);
    let owner = wallet(&rpc, KEYS[0]);
    for _ in 0..50 {
        if owner.get_chain_id().await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let base_fee = parse_ether("0.001").unwrap();
    let rng_args = OrvynRNG::constructorCall { owner_: deployer, committer_: relayer, treasury_: deployer, minParticipants_: 1, baseFee_: base_fee, callbackFeePerGas_: U256::from(1_000_000_000u64), nodesBps_: 7000, fulfillerBps_: 2000 };
    let rng_addr = deploy(&owner, bytecode("OrvynRNG.sol", "OrvynRNG"), rng_args.abi_encode()).await;

    // Orvyn's code on the local chain and contract.
    support::test_env(&[("BSC_RPC_URL", &rpc), ("ORVYN_RNG_CONTRACT", &rng_addr.to_string()), ("ORVYN_RNG_RELAYER_KEY", KEYS[1])]);
    let db = support::shared_db().await;
    let mut failed: Vec<String> = vec![];
    let mut ok = |name: &str, pass: bool, detail: String| {
        println!("{} {name}: {detail}", if pass { "ok  " } else { "FAIL" });
        if !pass {
            failed.push(name.into());
        }
    };

    // Two hosted nodes, registered on chain with Orvyn's proof builder.
    for (i, (agent_id, key, payee)) in [("agent-a", KEYS[2], node_a), ("agent-b", KEYS[3], node_b)].into_iter().enumerate() {
        sqlx::query("insert into agents (id, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status) values ($1, $1, 'N', '0x1', 'x', 'x', 'x', 'x', $2, '{}', 'live')")
            .bind(agent_id)
            .bind(format!("{payee:#x}"))
            .execute(db)
            .await
            .unwrap();
        let joined = rng::join_hosted(agent_id).await.unwrap();
        let public_key = joined["publicKey"].as_str().unwrap();
        let sealed: String = sqlx::query_scalar("select secret_encrypted from rng_nodes where agent_id = $1").bind(agent_id).fetch_one(db).await.unwrap();
        let secret = secrets::decrypt_secret(&sealed, &rng::aad(agent_id), secrets::SecretKind::AiKey).unwrap();
        let proof = chain::registration_proof(&secret, &format!("{payee:#x}"), public_key).unwrap();
        let pk: Bytes = hex::decode(bls::g2_eip(public_key).unwrap().trim_start_matches("0x")).unwrap().into();
        let c = OrvynRNG::new(rng_addr, wallet(&rpc, key));
        c.register(pk, payee, hex::decode(proof.trim_start_matches("0x")).unwrap().into()).value(parse_ether("0.1").unwrap()).send().await.unwrap().get_receipt().await.unwrap();
        sqlx::query("update rng_nodes set chain_index = $2 where agent_id = $1").bind(agent_id).bind(i as i32).execute(db).await.unwrap();
    }
    let rng_c = OrvynRNG::new(rng_addr, owner.clone());
    let nodes = rng_c.nodeCount().call().await.unwrap();
    ok("nodes register on chain with Orvyn's proofs", nodes == U256::from(2u8), nodes.to_string());

    // A developer's subscription and consumer; a roll in the minute before the round.
    rng_c.createSubscription().send().await.unwrap().get_receipt().await.unwrap();
    rng_c.fund(U256::from(1u8)).value(parse_ether("0.5").unwrap()).send().await.unwrap().get_receipt().await.unwrap();
    let dice_addr = deploy(&owner, bytecode("DiceRoller.sol", "DiceRoller"), DiceRoller::constructorCall { rng: rng_addr, subId: U256::from(1u8) }.abi_encode()).await;
    rng_c.setConsumer(U256::from(1u8), dice_addr, true).send().await.unwrap().get_receipt().await.unwrap();
    set_time(&owner, start - 20).await;
    let dice = DiceRoller::new(dice_addr, owner.clone());
    dice.roll().send().await.unwrap().get_receipt().await.unwrap();

    // Both nodes sign the round (as the orchestrator does), during the round.
    for agent_id in ["agent-a", "agent-b"] {
        let sealed: String = sqlx::query_scalar("select secret_encrypted from rng_nodes where agent_id = $1").bind(agent_id).fetch_one(db).await.unwrap();
        let secret = secrets::decrypt_secret(&sealed, &rng::aad(agent_id), secrets::SecretKind::AiKey).unwrap();
        sqlx::query("insert into rng_partials (round, agent_id, signature, created_at) values ($1, $2, $3, to_timestamp($4))")
            .bind(round as i32)
            .bind(agent_id)
            .bind(rng::sign_round(&secret, round).unwrap())
            .bind((start + 10) as f64)
            .execute(db)
            .await
            .unwrap();
    }

    // Close: the relayer commits; then drand: it fulfils.
    let close = rng::round_close(round);
    set_time(&owner, close + 2).await;
    let c = chain::relay_rounds((close + 2) as f64).await.unwrap();
    ok("the relayer commits the requested round inside the window", c["committed"] == 1, c.to_string());
    let drand_at = rng::drand::GENESIS + (rng::drand_round_for(round) - 1) * rng::drand::PERIOD;
    set_time(&owner, drand_at + 2).await;
    let f = chain::relay_rounds((drand_at + 2) as f64).await.unwrap();
    ok("and fulfils it once drand is out", f["fulfilled"] == 1, f.to_string());

    let on_chain: FixedBytes<32> = rng_c.randomness(round as u64).call().await.unwrap();
    rng::finalize_rounds((drand_at + 3) as f64).await.unwrap();
    let record = rng::round_proof(Some(round)).await.unwrap().unwrap_or(json!({}));
    ok("the on-chain output equals the off-chain record", record["output"].as_str() == Some(&hex::encode(on_chain)), format!("{on_chain} vs {}", record["output"]));
    ok("the off-chain record verifies too", record["verification"]["valid"] == true, record["verification"].to_string());
    let roll = dice.lastRoll(deployer).call().await.unwrap();
    ok("the dice got its roll", (1..=6).contains(&roll), roll.to_string());
    let each = base_fee * U256::from(7000u16) / U256::from(10_000u16) / U256::from(2u8);
    let (ea, eb) = (rng_c.earnings(node_a).call().await.unwrap(), rng_c.earnings(node_b).call().await.unwrap());
    ok("each agent earned its share of the fee", ea == each && eb == each, format!("{ea} {eb}"));

    // ---- Price feeds: the same staked nodes report BTC / USD on chain ----
    let feeds_addr = deploy(&owner, bytecode("OrvynPriceFeed.sol", "OrvynPriceFeed"), OrvynPriceFeed::constructorCall { owner_: deployer, network_: rng_addr, treasury_: deployer }.abi_encode()).await;
    let feeds = OrvynPriceFeed::new(feeds_addr, owner.clone());
    let pair = "BTC / USD";
    let feed_id = price::feed_id(pair);
    feeds.configureFeed(pair.into(), 2, 50, 3600, Address::ZERO, 0, base_fee, 8000).send().await.unwrap().get_receipt().await.unwrap();
    feeds.fund(feed_id).value(parse_ether("0.1").unwrap()).send().await.unwrap().get_receipt().await.unwrap();
    unsafe { std::env::set_var("ORVYN_PRICE_FEED_CONTRACT", feeds_addr.to_string()) };
    let at = |sec: i64| (rng::round_start(round) + sec) as f64;
    price::observe_hosted(at(5), |p| async move { (p == pair).then_some(61_234.5) }).await.unwrap();
    price::build_reports(at(31)).await.unwrap();
    price::sign_hosted(at(35)).await.unwrap();
    price::finalize_reports(at(52)).await.unwrap();
    let relayed = price::chain::relay_reports().await.unwrap();
    ok("the relayer submits the signed price report", relayed["submitted"] == 1, relayed.to_string());
    let latest = feeds.latest(feed_id).call().await.unwrap();
    let (on_price, on_round, signers) = (latest._0, latest._1, latest._3);
    ok("the contract holds the network's price", on_price.to_string() == price::to_e8(61_234.5).to_string() && on_round == round as u64 && signers == 2, format!("{on_price} round {on_round} signers {signers}"));
    let adapter_addr = deploy(&owner, bytecode("OrvynPriceFeed.sol", "OrvynFeedAdapter"), OrvynFeedAdapter::constructorCall { hub_: feeds_addr, feedId_: feed_id }.abi_encode()).await;
    let adapter = OrvynFeedAdapter::new(adapter_addr, owner.clone());
    let answer = adapter.latestRoundData().call().await.unwrap()._1;
    let description = adapter.description().call().await.unwrap();
    ok("a Chainlink-style consumer reads it", answer == on_price && description == pair, format!("{answer} {description}"));
    let paid = feeds.earnings(node_a).call().await.unwrap();
    ok("the signing agents are paid for the report", paid == base_fee * U256::from(8000u16) / U256::from(10_000u16) / U256::from(2u8), paid.to_string());
    let again = price::chain::relay_reports().await.unwrap();
    ok("nothing more to submit until the price moves", again["submitted"] == 0, again.to_string());
    assert!(failed.is_empty(), "failed: {failed:?}");
}
