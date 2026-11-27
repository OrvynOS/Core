//! OrvynID on chain, end to end on a local chain (anvil, chain id 56) with the real contract
//! (contracts/out, from `forge build`) behind its proxy, and a stand-in Agent ID registry:
//! orvyn-core's sync issues the agent's non-transferable OrvynID to its wallet, links its ERC-8004
//! entry, records its Agent ID (checked live by the contract), then follows a new handle, a rotated
//! wallet and a lost Agent ID; a second sync with nothing new changes nothing.
//!
//! Needs Foundry: `forge build` in contracts/, anvil on PATH (or ANVIL=path), then
//! `cargo test -p orvyn-core --test orvyn_id_chain -- --ignored --nocapture`.
mod support;

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolCall, SolConstructor};
use serde_json::Value;

sol! {
    #[sol(rpc)]
    contract MockAgentIds {
        function set(string name, address holder);
    }
    #[sol(rpc)]
    contract OrvynIDView {
        function initialize(address owner_, address agentIdRegistry_, string rootDomain_);
        function setIssuer(address issuer, bool allowed);
        function ownerOf(uint256 tokenId) view returns (address);
        function tokenURI(uint256 tokenId) view returns (string);
        function locked(uint256 tokenId) view returns (bool);
        function agentIdOf(uint256 tokenId) view returns (string name, uint256 agentIdToken, address holder, bool heldNow);
        function transferFrom(address from, address to, uint256 tokenId);
    }
    contract ERC1967Proxy {
        constructor(address implementation, bytes data);
    }
}

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

#[tokio::test]
#[ignore = "needs Foundry (anvil, and contracts built with forge build)"]
async fn orvyn_id_follows_the_agent() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let rpc = format!("http://127.0.0.1:{port}");
    let _anvil = Anvil(
        std::process::Command::new(std::env::var("ANVIL").unwrap_or_else(|_| "anvil".into()))
            .args(["--port", &port.to_string(), "--chain-id", "56", "--silent"])
            .spawn()
            .expect("anvil (install Foundry, or set ANVIL)"),
    );
    let accounts: Vec<Address> = KEYS.iter().map(|k| k.parse::<PrivateKeySigner>().unwrap().address()).collect();
    let (deployer, operator, agent_wallet, creator) = (accounts[0], accounts[1], accounts[2], accounts[3]);
    let owner = wallet(&rpc, KEYS[0]);
    for _ in 0..50 {
        if owner.get_chain_id().await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // The Agent ID registry stand-in, OrvynID behind its proxy, and Orvyn's operator as issuer.
    let agent_ids = deploy(&owner, bytecode("OrvynID.t.sol", "MockAgentIds"), vec![]).await;
    let implementation = deploy(&owner, bytecode("OrvynID.sol", "OrvynID"), vec![]).await;
    let init = OrvynIDView::initializeCall { owner_: deployer, agentIdRegistry_: agent_ids, rootDomain_: "orvyn.cash".into() }.abi_encode();
    let proxy = deploy(&owner, bytecode("ERC1967Proxy.sol", "ERC1967Proxy"), ERC1967Proxy::constructorCall { implementation, data: init.into() }.abi_encode()).await;
    let id = OrvynIDView::new(proxy, owner.clone());
    id.setIssuer(operator, true).send().await.unwrap().get_receipt().await.unwrap();
    MockAgentIds::new(agent_ids, owner.clone()).set("atlas".into(), creator).send().await.unwrap().get_receipt().await.unwrap();

    support::test_env(&[("BSC_RPC_URL", &rpc), ("ORVYN_OPERATOR_PRIVATE_KEY", KEYS[1])]);
    unsafe { std::env::set_var("ORVYN_ID_CONTRACT", proxy.to_string()) };
    let db = support::shared_db().await;
    sqlx::query("insert into agents (id, handle, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status, agent_id_name, agent_id_verified_at, erc8004_agent_id) values ('atlas-1', 'atlas', 'Atlas', 'ATLAS', $1, 'x', 'x', 'x', 'x', $2, '{}', 'live', 'atlas', now(), '7')")
        .bind(format!("{creator:#x}"))
        .bind(format!("{agent_wallet:#x}"))
        .execute(db)
        .await
        .unwrap();
    let agent = || async { sqlx::query_as::<_, orvyn_core::db::Agent>("select * from agents where id = 'atlas-1'").fetch_one(db).await.unwrap() };

    // First sync: issued to the agent's wallet, ERC-8004 linked, Agent ID recorded.
    let changed = orvyn_core::orvyn_id::sync(&agent().await).await.unwrap();
    assert_eq!(changed, ["issued", "erc8004", "agent-id"]);
    let token = orvyn_core::orvyn_id::token_of("atlas-1").await.unwrap();
    assert_eq!(token, U256::from(1));
    assert_eq!(id.ownerOf(token).call().await.unwrap(), agent_wallet);
    assert!(id.locked(token).call().await.unwrap());
    assert_eq!(id.tokenURI(token).call().await.unwrap(), "https://atlas.orvyn.cash/.well-known/orvyn-id.json");
    let a = id.agentIdOf(token).call().await.unwrap();
    assert_eq!((a.name.as_str(), a.holder, a.heldNow), ("atlas", creator, true));
    // Nothing new: nothing sent.
    assert!(orvyn_core::orvyn_id::sync(&agent().await).await.unwrap().is_empty());
    // The OrvynID document names the NFT.
    let doc = orvyn_core::identity::identity_document(&agent().await).await.unwrap();
    assert_eq!(doc["document"]["nft"]["tokenId"], "1");

    // The agent can't transfer it.
    let agent_signer = wallet(&rpc, KEYS[2]);
    let transfer = OrvynIDView::new(proxy, agent_signer).transferFrom(agent_wallet, deployer, token).send().await;
    assert!(transfer.is_err(), "soulbound");

    // A new handle, a rotated wallet and a lost Agent ID follow on the next sync.
    let new_wallet: Address = "0x00000000000000000000000000000000000000a7".parse().unwrap();
    sqlx::query("update agents set handle = 'atlas-two', evm_address = $1, agent_id_verified_at = null where id = 'atlas-1'").bind(format!("{new_wallet:#x}")).execute(db).await.unwrap();
    let changed = orvyn_core::orvyn_id::sync(&agent().await).await.unwrap();
    assert_eq!(changed, ["handle", "wallet", "agent-id-cleared"]);
    assert_eq!(id.ownerOf(token).call().await.unwrap(), new_wallet);
    assert_eq!(id.tokenURI(token).call().await.unwrap(), "https://atlas-two.orvyn.cash/.well-known/orvyn-id.json");
    assert_eq!(id.agentIdOf(token).call().await.unwrap().name, "");
    println!("ok: OrvynID #{token} issued, linked, recorded, moved and cleared");
}
