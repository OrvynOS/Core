//! Checking signatures: EOAs by recovery, smart wallets (ERC-1271, and ERC-6492 for wallets not
//! deployed yet) with the ERC-6492 universal validator, called without deploying it (eth_call of
//! its creation code). The validator's bytecode is viem's (MIT), from the ERC-6492 reference.

use alloy::primitives::{Address, Bytes, Signature, B256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolValue;

use crate::error::Result;

const VALIDATOR: &str = include_str!("erc6492-validator.hex");

/// The signer of a 32-byte hash, if the signature is a plain ECDSA one.
pub fn recover(hash: B256, signature: &[u8]) -> Option<Address> {
    let sig = Signature::try_from(signature).ok()?;
    sig.recover_address_from_prehash(&hash).ok()
}

/// Whether `signature` is `address`'s signature of `hash`: an EOA's, or a smart wallet's (checked
/// on chain). RPC failures are errors; a signature that doesn't check out is `false`.
pub async fn verify_hash(address: Address, hash: B256, signature: &[u8]) -> Result<bool> {
    if recover(hash, signature) == Some(address) {
        return Ok(true);
    }
    let code = hex::decode(VALIDATOR.trim()).expect("validator bytecode");
    let mut data = code;
    data.extend((address, hash, Bytes::copy_from_slice(signature)).abi_encode_params());
    let call = TransactionRequest::default().input(Bytes::from(data).into());
    match super::bsc().call(call).await {
        // The validator's creation code returns one byte: 0x01 valid.
        Ok(out) => Ok(out.last() == Some(&1)),
        // A revert means the signature isn't valid (the validator reverts on bad input).
        Err(e) if e.to_string().contains("revert") => Ok(false),
        Err(e) => Err(super::rpc_error(e)),
    }
}

/// EIP-191 hash of a text message ("\x19Ethereum Signed Message:\n<len><message>").
pub fn message_hash(message: &str) -> B256 {
    alloy::primitives::eip191_hash_message(message.as_bytes())
}
