//! Orvyn's backend core in Rust: the API, the orchestrator, agents and the operator tool run on it.
//! It kept the TypeScript core's database and formats (secrets, tokens, signatures), so data the
//! TypeScript backend wrote reads as it is (tests/vectors hold that core's outputs).

pub mod accounts;
pub mod agent;
pub mod agentid;
pub mod api_clients;
pub mod aster;
pub mod ai;
pub mod auth;
pub mod billing;
pub mod binance;
pub mod cards;
pub mod credits;
pub mod data;
pub mod binancepay;
pub mod bliss;
pub mod db;
pub mod env;
pub mod environment;
pub mod erc8004;
pub mod error;
pub mod evm;
pub mod external_agents;
pub mod games;
pub mod hires;
pub mod hosting;
pub mod http;
pub mod identity;
pub mod inference;
pub mod invoices;
pub mod jobs;
pub mod launchpad;
pub mod lifecycle;
pub mod oauth;
pub mod mail;
pub mod orvynpay;
pub mod portfolio;
pub mod price;
pub mod privy;
pub mod public;
pub mod rng;
pub mod push;
pub mod secrets;
pub mod service;
pub mod studio;
pub mod subscription;
pub mod tiers;
pub mod views;
pub mod vyn;
pub mod wallet_policy;
pub mod wallets;
pub mod withdraw;
pub mod worker;
pub mod x402;

pub use error::{Error, Kind, Result};
