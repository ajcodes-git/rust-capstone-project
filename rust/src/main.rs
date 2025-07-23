#![allow(unused)]
use bitcoin::hex::DisplayHex;
use bitcoincore_rpc::bitcoin::Amount;
use bitcoincore_rpc::{Auth, Client, RpcApi};
use serde::Deserialize;
use serde_json::json;
use std::fs::File;
use std::io::Write;

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

    // Helper to call wallet-specific RPC
    let miner_rpc = Client::new(
        &format!("{RPC_URL}/wallet/Miner"),
        Auth::UserPass(RPC_USER.to_owned(), RPC_PASS.to_owned()),
    )?;
    let trader_rpc = Client::new(
        &format!("{RPC_URL}/wallet/Trader"),
        Auth::UserPass(RPC_USER.to_owned(), RPC_PASS.to_owned()),
    )?;

    // 1. Create/load wallets
    for wallet in ["Miner", "Trader"] {
        let res = rpc.call::<serde_json::Value>("createwallet", &[json!(wallet)]);
        if let Err(e) = &res {
            if !e.to_string().contains("already exists") {
                panic!("Failed to create wallet: {e}");
            }
            // Try loading if not loaded
            let _ = rpc.call::<serde_json::Value>("loadwallet", &[json!(wallet)]);
        }
    }

    // 2. Generate address for mining reward in Miner wallet
    let mining_addr = miner_rpc.call::<String>("getnewaddress", &[json!("Mining Reward")])?;

    // 3. Mine blocks until positive balance but only enough to have ONE large UTXO
    // Coinbase rewards require 100 blocks to mature before spendable
    let mut blocks_mined = 0;
    let mut miner_balance = miner_rpc.get_balance(None, None)?;

    // Mine exactly 101 blocks to get one mature UTXO
    while blocks_mined < 101 || miner_balance.to_btc() <= 0.0 {
        miner_rpc
            .call::<Vec<String>>("generatetoaddress", &[json!(1), json!(mining_addr.clone())])?;
        blocks_mined += 1;
        miner_balance = miner_rpc.get_balance(None, None)?;

        // Stop after 101 blocks to ensure we have only one spendable UTXO
        if blocks_mined >= 101 && miner_balance.to_btc() > 0.0 {
            break;
        }
    }
    // Coinbase rewards are only spendable after 100 blocks (maturity)
    // This is to prevent chain reorganizations from invalidating coinbase spends.

    println!("Blocks mined until positive balance: {blocks_mined}");
    println!("Miner wallet balance: {} BTC", miner_balance.to_btc());

    // 4. Generate Trader receiving address
    let trader_addr = trader_rpc.call::<String>("getnewaddress", &[json!("Received")])?;

    // 5. Send 20 BTC from Miner to Trader
    let txid =
        miner_rpc.call::<String>("sendtoaddress", &[json!(trader_addr.clone()), json!(20.0)])?;
    println!("Sent 20 BTC from Miner to Trader. TXID: {txid}");

    // 6. Fetch unconfirmed transaction from mempool
    let mempool_entry =
        miner_rpc.call::<serde_json::Value>("getmempoolentry", &[json!(txid.clone())])?;
    println!("Mempool entry: {mempool_entry:?}");

    // 7. Mine 1 block to confirm transaction
    miner_rpc.call::<Vec<String>>("generatetoaddress", &[json!(1), json!(mining_addr.clone())])?;

    // 8. Extract transaction details with proper error handling
    let tx_info = miner_rpc.call::<serde_json::Value>(
        "gettransaction",
        &[json!(txid.clone()), json!(null), json!(true)],
    )?;
    let decoded = tx_info["decoded"].clone();
    let blockheight = tx_info["blockheight"].as_i64().unwrap_or(0);
    let blockhash = tx_info["blockhash"].as_str().unwrap_or("unknown");
    let fee = tx_info["fee"].as_f64().unwrap_or(0.0);

    // Find input address and amount - get actual input transaction details
    let vin = decoded["vin"].as_array().unwrap();
    let input_txid = vin[0]["txid"].as_str().unwrap();
    let input_vout = vin[0]["vout"].as_u64().unwrap() as usize;

    // Get the previous transaction to find the input details
    let input_tx = miner_rpc.call::<serde_json::Value>(
        "gettransaction",
        &[json!(input_txid), json!(null), json!(true)],
    )?;
    let input_decoded = input_tx["decoded"].clone();
    let input_vouts = input_decoded["vout"].as_array().unwrap();
    let input_vout_obj = &input_vouts[input_vout];

    // Safely extract input address and amount
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

    // Find output addresses and amounts - handle all edge cases safely
    let vout = decoded["vout"].as_array().unwrap();
    let mut trader_output_address = "";
    let mut trader_output_amount = 0.0;
    let mut miner_change_address = "";
    let mut miner_change_amount = 0.0;

    for out in vout {
        // Only process outputs with valid value field
        if let Some(value) = out.get("value").and_then(|v| v.as_f64()) {
            // Only process outputs with valid address field
            if let Some(addr_val) = out["scriptPubKey"].get("address") {
                if let Some(addr) = addr_val.as_str() {
                    if addr == trader_addr {
                        trader_output_address = addr;
                        trader_output_amount = value;
                    } else if addr != trader_addr {
                        // Any address that's not the trader address is considered change
                        miner_change_address = addr;
                        miner_change_amount = value;
                    }
                }
            }
        }
    }

    // 9. Write output to ../out.txt
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
