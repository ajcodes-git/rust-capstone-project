#![allow(unused)]
use bitcoincore_rpc::bitcoin::Amount;
use bitcoincore_rpc::bitcoin::{Address, Network};
use bitcoincore_rpc::jsonrpc;
use bitcoincore_rpc::Error as RpcError;
use bitcoincore_rpc::{Auth, Client, RpcApi};
use serde::Deserialize;
use serde_json::json;
use std::fs::File;
use std::io::Write;
use std::str::FromStr;

// Node access params
const RPC_URL: &str = "http://127.0.0.1:18443"; // Default regtest RPC port
const RPC_USER: &str = "alice";
const RPC_PASS: &str = "password";

// You can use calls not provided in RPC lib API using the generic `call` function.
// An example of using the `send` RPC call, which doesn't have exposed API.
// You can also use serde_json `Deserialize` derivation to capture the returned json result.
fn send(rpc: &Client, addr: &str) -> bitcoincore_rpc::Result<String> {
    let args = [
        json!([{addr : 100 }]), // recipient address
        json!(null),            // conf target
        json!(null),            // estimate mode
        json!(null),            // fee rate in sats/vb
        json!(null),            // Empty option object
    ];

    #[derive(Deserialize)]
    struct SendResult {
        complete: bool,
        txid: String,
    }
    let send_result = rpc.call::<SendResult>("send", &args)?;
    assert!(send_result.complete);
    Ok(send_result.txid)
}

fn main() -> bitcoincore_rpc::Result<()> {
    // Connect to Bitcoin Core RPC
    let rpc = Client::new(
        RPC_URL,
        Auth::UserPass(RPC_USER.to_owned(), RPC_PASS.to_owned()),
    )?;

    // Get blockchain info
    let blockchain_info = rpc.get_blockchain_info()?;
    println!("Blockchain Info: {:?}", blockchain_info);

    // ================= 1. Ensure both "Miner" and "Trader" wallets are available by creating or loading them=====================
    for wallet in ["Miner", "Trader"] {
        let response = rpc.call::<serde_json::Value>("createwallet", &[json!(wallet)]);
        if let Err(e) = &response {
            if !e.to_string().contains("already exists") {
                panic!("Failed to create wallet: {e}");
            }
            // If the wallet already exists, attempt to load it in case it's not currently loaded
            let _ = rpc.call::<serde_json::Value>("loadwallet", &[json!(wallet)]);
        }
    }

    // Initialize RPC clients for wallet-specific operations (Miner and Trader wallets)
    let miner_rpc = Client::new(
        &format!("{RPC_URL}/wallet/Miner"),
        Auth::UserPass(RPC_USER.to_owned(), RPC_PASS.to_owned()),
    )?;

    let trader_rpc = Client::new(
        &format!("{RPC_URL}/wallet/Trader"),
        Auth::UserPass(RPC_USER.to_owned(), RPC_PASS.to_owned()),
    )?;

    //Generate a new address in the "Miner" wallet to receive mining rewards
    let mining_address_str =
        miner_rpc.call::<String>("getnewaddress", &[json!("Mining Reward")])?;
    let mining_address = Address::from_str(&mining_address_str).map_err(|e| {
        eprintln!("Address parse error: {e}");
        RpcError::UnexpectedStructure
    })?;
    let mining_address = mining_address
        .require_network(Network::Regtest)
        .map_err(|e| {
            eprintln!("Network error: {e}");
            RpcError::UnexpectedStructure
        })?;

    // ============== 2. Generate initial balance by mining 103 blocks to the Miner address=====================
    // 100 blocks for coinbase maturity + 3 for spendable balance
    // 103 blocks: Coinbase transactions require 100 confirmations before the mined BTC can be spent.
    rpc.generate_to_address(103, &mining_address)?;

    // =================== 3. Generate a receiving address in the Trader wallet=========================
    let trader_address = trader_rpc.call::<String>("getnewaddress", &[json!("Trader Address")])?;

    // ================= 4. send 20 BTC from Miner to Trader====================
    let txid = miner_rpc.call::<String>("sendtoaddress", &[json!(trader_address), json!(20.0)])?;
    println!("Transaction ID: {}", txid);

    // ================ 5. Check if transaction is in the mempool=========================
    let mempool_entry =
        miner_rpc.call::<serde_json::Value>("getmempoolentry", &[json!(txid.clone())])?;
    println!("Mempool entry: {mempool_entry:?}");

    // ================ 6. Mine 1 block to confirm the transaction===========================
    let _ = rpc.generate_to_address(1, &mining_address)?;

    // ============== 7. Retrieve and safely extract relevant transaction details from the Miner wallet=====================
    let tx_info = miner_rpc.call::<serde_json::Value>(
        "gettransaction",
        &[json!(txid.clone()), json!(null), json!(true)],
    )?;
    let decoded = tx_info["decoded"].clone();
    let blockheight = tx_info["blockheight"].as_i64().unwrap_or(0);
    let blockhash = tx_info["blockhash"].as_str().unwrap_or("unknown");
    let fee = tx_info["fee"].as_f64().unwrap_or(0.0);

    // Extract input transaction ID and output index from the decoded transaction
    let vin = decoded["vin"].as_array().unwrap();
    let input_txid = vin[0]["txid"].as_str().unwrap();
    let input_vout = vin[0]["vout"].as_u64().unwrap() as usize;

    // Fetch the previous transaction to trace back the source of the input funds
    let input_tx = miner_rpc.call::<serde_json::Value>(
        "gettransaction",
        &[json!(input_txid), json!(null), json!(true)],
    )?;
    let input_decoded = input_tx["decoded"].clone();
    let input_vouts = input_decoded["vout"].as_array().unwrap();
    let input_vout_obj = &input_vouts[input_vout];

    // Extract the originating address of the input, falling back to script info if address is unavailable
    let miner_input_address = if let Some(addr_val) = input_vout_obj["scriptPubKey"].get("address")
    {
        if let Some(addr_str) = addr_val.as_str() {
            addr_str.to_string()
        } else {
            input_vout_obj["scriptPubKey"]["asm"]
                .as_str()
                .unwrap_or("unknown")
                .to_string()
        }
    } else {
        input_vout_obj["scriptPubKey"]["asm"]
            .as_str()
            .unwrap_or("unknown")
            .to_string()
    };

    let miner_input_amount = input_vout_obj["value"].as_f64().unwrap_or(0.0);

    // Parse transaction outputs to identify recipient (Trader) and change (Miner) addresses and amounts
    let vout = decoded["vout"].as_array().unwrap();
    let mut trader_output_address = "";
    let mut trader_output_amount = 0.0;
    let mut miner_change_address = "";
    let mut miner_change_amount = 0.0;

    for out in vout {
        // Safely extract the output amount
        if let Some(value) = out.get("value").and_then(|v| v.as_f64()) {
            // Safely extract the destination address, if available
            if let Some(address_value) = out["scriptPubKey"].get("address") {
                if let Some(address) = address_value.as_str() {
                    if address == trader_address {
                        // This is the recipient (Trader) output
                        trader_output_address = address;
                        trader_output_amount = value;
                    } else if address != trader_address {
                        // Any address that's not the Trader's is treated as change (likely to Miner)
                        miner_change_address = address;
                        miner_change_amount = value;
                    }
                }
            }
        }
    }

    // ========== 8. Write all extracted transaction details to ../out.txt in the required output format==============
    let mut file = File::create("../out.txt").expect("Unable to create out.txt");
    writeln!(file, "{txid}")?;
    writeln!(file, "{miner_input_address}")?;
    writeln!(file, "{miner_input_amount}")?;
    writeln!(file, "{trader_output_address}")?;
    writeln!(file, "{trader_output_amount}")?;
    writeln!(file, "{miner_change_address}")?;
    writeln!(file, "{miner_change_amount}")?;
    writeln!(file, "{fee}")?;
    writeln!(file, "{blockheight}")?;
    writeln!(file, "{blockhash}")?;

    Ok(())
}
