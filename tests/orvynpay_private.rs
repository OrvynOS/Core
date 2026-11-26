//! OrvynPay private mode end to end, with real Groth16 proofs from the real circuit
//! (contracts/circuits/build.sh first). A stand-in for OrvynShieldedPool applies every check
//! `transact` makes (known roots, unspent and distinct nullifiers, extDataHash, publicAmount, the
//! proof against the verifying key in the contract's signal order), then inserts the outputs and
//! keeps the encrypted notes, so scanning sees what it would on chain.
//!
//! Alice deposits 10 USDT, pays Bob 4 privately, Bob finds the note by scanning and withdraws it;
//! double spends, tampered data, forged amounts, unregistered keys and a key tree that isn't the
//! registry's are refused.
//!
//! Slow (several proofs): `cargo test -p orvyn-core --release --test orvynpay_private -- --ignored`.
mod support;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use alloy::primitives::{Address, I256, U256};
use orvyn_core::orvynpay::crypto::{encrypt_note, new_shielded_keys, random_field, MerkleTree, Note, ShieldedKeys};
use orvyn_core::orvynpay::transaction::{build_transaction, ext_data_hash, prove_transaction, public_amount_of, signals_of, verify_proof, BuildInput, KEY_LEVELS, NOTE_LEVELS};
use orvyn_core::orvynpay::{asset_of, private_transaction, shielded_balance, unspent_notes, PoolView, PrivateKind, PrivateTx, RegisteredKey};

struct Pool {
    view: PoolView,
    roots: Vec<U256>,
    key_roots: Vec<U256>,
    withdrawn: HashMap<Address, U256>,
    zkey: PathBuf,
}

impl Pool {
    fn register(&mut self, account: Address, keys: &ShieldedKeys) {
        self.view.key_tree.insert(&[keys.public_key]);
        self.key_roots.push(self.view.key_tree.root());
        self.view.keys_by_account.insert(format!("{account:#x}"), RegisteredKey { shielded_key: keys.public_key, encryption_key: keys.encryption_public.clone() });
    }

    /// OrvynShieldedPool.transact's checks, then its effects.
    fn transact(&mut self, tx: &PrivateTx) -> Result<(), String> {
        let p = &tx.proof;
        if !self.roots.contains(&p.root) {
            return Err("UnknownRoot".into());
        }
        if !self.key_roots.contains(&p.keyRoot) {
            return Err("UnknownKeyRoot".into());
        }
        let [n0, n1] = p.inputNullifiers;
        if n0 == n1 {
            return Err("DuplicateNullifier".into());
        }
        if self.view.spent.contains(&n0) || self.view.spent.contains(&n1) {
            return Err("NullifierSpent".into());
        }
        if ext_data_hash(&tx.ext) != p.extDataHash {
            return Err("BadExtData".into());
        }
        if public_amount_of(tx.ext.extAmount, tx.ext.fee).map_err(|e| e.message)? != p.publicAmount {
            return Err("BadPublicAmount".into());
        }
        assert_eq!(signals_of(p).len(), 9, "the contract's nine public signals");
        if !verify_proof(&self.zkey, p).map_err(|e| e.message)? {
            return Err("BadProof".into());
        }
        self.view.spent.extend([n0, n1]);
        let index = self.view.note_tree.leaves().len();
        self.view.note_tree.insert(&p.outputCommitments);
        self.roots.push(self.view.note_tree.root());
        self.view.encrypted.insert(index, (p.outputCommitments[0], tx.ext.encryptedOutput1.to_string()));
        self.view.encrypted.insert(index + 1, (p.outputCommitments[1], tx.ext.encryptedOutput2.to_string()));
        if tx.ext.extAmount < I256::ZERO {
            *self.withdrawn.entry(tx.ext.recipient).or_default() += tx.ext.extAmount.unsigned_abs();
        }
        Ok(())
    }
}

#[tokio::test]
#[ignore = "slow: real Groth16 proofs (needs contracts/circuits/build)"]
async fn private_payment_cycle() {
    support::test_env(&[]);
    let build = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/circuits/build");
    let (wasm, zkey) = (build.join("transaction_js/transaction.wasm"), build.join("transaction.zkey"));
    assert!(wasm.exists() && zkey.exists(), "build the circuit first: contracts/circuits/build.sh");
    unsafe {
        std::env::set_var("ORVYN_SHIELDED_WASM", &wasm);
        std::env::set_var("ORVYN_SHIELDED_ZKEY", &zkey);
    }
    let view = PoolView { note_tree: MerkleTree::new(NOTE_LEVELS, vec![]), key_tree: MerkleTree::new(KEY_LEVELS, vec![]), encrypted: HashMap::new(), spent: HashSet::new(), keys_by_account: HashMap::new(), to_block: 0 };
    let mut pool = Pool { roots: vec![view.note_tree.root()], key_roots: vec![view.key_tree.root()], view, withdrawn: HashMap::new(), zkey };
    let (alice_at, bob_at, eve_at): (Address, Address, Address) =
        ("0x000000000000000000000000000000000000a11c".parse().unwrap(), "0x0000000000000000000000000000000000000b0b".parse().unwrap(), "0x0000000000000000000000000000000000000e5e".parse().unwrap());
    let (alice, bob, eve) = (new_shielded_keys(), new_shielded_keys(), new_shielded_keys());
    pool.register(alice_at, &alice);
    pool.register(bob_at, &bob);
    let usdt_asset = asset_of("USDT").unwrap();
    let usdt = |n: u64| U256::from(n) * U256::from(10u64).pow(U256::from(18u8));
    let balance = |pool: &Pool, keys: &ShieldedKeys| shielded_balance(&unspent_notes(&pool.view, keys), usdt_asset);

    // Deposit.
    let started = std::time::Instant::now();
    let deposit = private_transaction(PrivateKind::Deposit, &alice, &pool.view, "USDT", usdt(10), alice_at, None).await.unwrap();
    println!("deposit proof built in {} ms", started.elapsed().as_millis());
    pool.transact(&deposit).expect("the deposit passes the pool's checks");
    assert_eq!(balance(&pool, &alice), usdt(10), "Alice sees 10 USDT shielded");
    assert!(unspent_notes(&pool.view, &bob).is_empty(), "Bob sees nothing");

    println!("{} s: deposit accepted", started.elapsed().as_secs());
    // A private payment, Alice to Bob.
    let transfer = private_transaction(PrivateKind::Transfer, &alice, &pool.view, "USDT", usdt(4), alice_at, Some(bob_at)).await.unwrap();
    assert!(transfer.ext.extAmount == I256::ZERO && transfer.ext.recipient == Address::ZERO, "a private payment moves nothing in or out");
    pool.transact(&transfer).unwrap();
    assert_eq!(balance(&pool, &alice), usdt(6), "Alice has 6 USDT left (change note)");
    assert_eq!(balance(&pool, &bob), usdt(4), "Bob found 4 USDT by scanning");
    assert!(pool.transact(&transfer).is_err(), "replaying it (a double spend) is refused");

    println!("{} s: transfer done", started.elapsed().as_secs());
    // Tampering.
    let mut tx = private_transaction(PrivateKind::Withdraw, &alice, &pool.view, "USDT", usdt(1), alice_at, None).await.unwrap();
    tx.ext.recipient = eve_at;
    assert_eq!(pool.transact(&tx), Err("BadExtData".into()), "a tampered recipient no longer matches the proof");
    let mut tx = private_transaction(PrivateKind::Withdraw, &alice, &pool.view, "USDT", usdt(1), alice_at, None).await.unwrap();
    tx.ext.extAmount = -I256::from_raw(usdt(5));
    tx.proof.extDataHash = ext_data_hash(&tx.ext);
    tx.proof.publicAmount = public_amount_of(tx.ext.extAmount, U256::ZERO).unwrap();
    assert_eq!(pool.transact(&tx), Err("BadProof".into()), "a forged public amount (withdrawing more than proven) fails the proof");
    println!("{} s: tampering checked", started.elapsed().as_secs());
    assert!(private_transaction(PrivateKind::Transfer, &alice, &pool.view, "USDT", usdt(1), alice_at, Some(eve_at)).await.is_err(), "paying a key that isn't registered");
    assert!(private_transaction(PrivateKind::Transfer, &alice, &pool.view, "USDT", usdt(7), alice_at, Some(bob_at)).await.is_err(), "spending more than the shielded balance");

    println!("{} s: refusals checked", started.elapsed().as_secs());
    // Bypass the SDK's checks: pay Eve against a key tree that includes her, proved for real.
    let fake_keys = MerkleTree::new(KEY_LEVELS, pool.view.key_tree.leaves().iter().copied().chain([eve.public_key]).collect());
    let input = unspent_notes(&pool.view, &alice).into_iter().next().unwrap();
    let note = |amount: U256, owner: U256| Note { amount, public_key: owner, blinding: random_field(), asset: usdt_asset, index: None };
    let outputs = vec![note(usdt(1), eve.public_key), note(input.amount - usdt(1), alice.public_key)];
    let encrypted = [encrypt_note(&outputs[0], &eve.encryption_public).unwrap(), encrypt_note(&outputs[1], &alice.encryption_public).unwrap()];
    let built = build_transaction(BuildInput {
        private_key: alice.private_key,
        public_key: alice.public_key,
        inputs: vec![input],
        outputs,
        encrypted,
        ext_amount: I256::ZERO,
        asset: usdt_asset,
        recipient: None,
        relayer: None,
        fee: U256::ZERO,
        note_tree: &pool.view.note_tree,
        key_tree: &fake_keys,
    })
    .unwrap();
    let forged = match prove_transaction(&built).await {
        Ok(proof) => pool.transact(&PrivateTx { proof, ext: built.ext, value: U256::ZERO }),
        Err(e) => Err(e.message),
    };
    assert!(forged.is_err(), "a payment to a key outside the registry is refused: {forged:?}");

    println!("{} s: fake key tree refused", started.elapsed().as_secs());
    // Bob withdraws.
    let withdraw = private_transaction(PrivateKind::Withdraw, &bob, &pool.view, "USDT", usdt(4), bob_at, None).await.unwrap();
    pool.transact(&withdraw).unwrap();
    assert_eq!(pool.withdrawn.get(&bob_at), Some(&usdt(4)), "Bob withdrew 4 USDT to his wallet");
    assert_eq!(balance(&pool, &bob), U256::ZERO, "Bob's shielded balance is 0");
    assert_eq!(balance(&pool, &alice), usdt(6), "Alice still has 6");
}
