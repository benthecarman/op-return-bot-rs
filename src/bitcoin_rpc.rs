use std::cmp::Reverse;
use std::str::FromStr;

use bitcoin::{
    Address, Amount, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid,
    Witness, absolute, consensus, opcodes,
    script::{Builder, PushBytesBuf},
    transaction,
};
use reqwest::Client;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use url::Url;

use crate::{AppError, AppResult, config::BitcoinConfig};

/// The wallet label that marks addresses created for `OP_RETURN` payments.
pub const ADDRESS_LABEL: &str = "OP_RETURN Bot";
/// Address type for change and receiving addresses. This matches the
/// addresses that the original service created.
const ADDRESS_TYPE: &str = "bech32m";
const DUST_SATS: u64 = 330;
/// Weight of the witness marker, flag, and a P2WPKH witness. Taproot key
/// path witnesses are smaller, so this estimate never under-counts.
const WITNESS_WEIGHT_ESTIMATE: usize = 110;
const LISTED_TRANSACTIONS: u32 = 1_000;
const RPC_INVALID_ADDRESS_OR_KEY: i64 = -5;

#[derive(Clone)]
pub struct BitcoinClient {
    http: Client,
    rpc_url: Url,
    rpc_user: String,
    rpc_password: String,
    network: Network,
    sending_wallet: String,
    receiving_wallet: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChainInfo {
    pub blocks: u64,
    pub chain: String,
}

/// One payment to a receiving address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedPayment {
    pub address: String,
    pub amount_sats: u64,
    pub txid: String,
    pub vout: u32,
    /// Negative when the transaction conflicts with a confirmed transaction.
    pub confirmations: i64,
}

/// The wallet that holds the input of an `OP_RETURN` transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wallet {
    Sending,
    Receiving,
}

/// How an `OP_RETURN` transaction is funded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Funding {
    /// Spend an output from the sending wallet. Confirmed outputs come
    /// first, then unconfirmed ones, as in the Scala service.
    SendingWallet,
    /// Spend the on-chain payment itself from the receiving wallet. A
    /// replaced payment then also invalidates the `OP_RETURN` transaction.
    PaymentOutput {
        txid: String,
        vout: u32,
        amount_sats: u64,
    },
}

#[derive(Clone, Debug)]
pub struct SignedOpReturn {
    pub transaction: Transaction,
    pub fee_sats: u64,
    pub wallet: Wallet,
}

#[derive(Debug, Deserialize)]
struct RpcEnvelope<T> {
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

enum RpcFailure {
    Rpc { code: i64, message: String },
    Other(AppError),
}

impl From<RpcFailure> for AppError {
    fn from(failure: RpcFailure) -> Self {
        match failure {
            RpcFailure::Rpc { code, message } => Self::Upstream(message_with_code(code, &message)),
            RpcFailure::Other(error) => error,
        }
    }
}

fn message_with_code(code: i64, message: &str) -> String {
    format!("error {code}: {message}")
}

#[derive(Debug, Deserialize)]
struct UnspentOutput {
    txid: String,
    vout: u32,
    amount: serde_json::Number,
    confirmations: u64,
    #[serde(default)]
    spendable: bool,
}

#[derive(Debug, Deserialize)]
struct SignedTransaction {
    hex: String,
    complete: bool,
    #[serde(default)]
    errors: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct WalletTransaction {
    txid: String,
    #[serde(default)]
    confirmations: i64,
    #[serde(default)]
    hex: String,
    details: Vec<WalletTransactionDetail>,
}

#[derive(Debug, Deserialize)]
struct WalletTransactionDetail {
    address: Option<String>,
    category: String,
    amount: serde_json::Number,
    vout: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ListedTransaction {
    address: Option<String>,
    category: String,
    amount: serde_json::Number,
    vout: Option<u32>,
    txid: Option<String>,
    #[serde(default)]
    confirmations: i64,
}

impl BitcoinClient {
    pub async fn connect(config: &BitcoinConfig) -> AppResult<Self> {
        let rpc_password = tokio::fs::read_to_string(&config.rpc_password_file)
            .await
            .map_err(|error| {
                AppError::Config(format!(
                    "could not read Bitcoin RPC password {}: {error}",
                    config.rpc_password_file.display()
                ))
            })?;
        let client = Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .no_proxy()
                .build()
                .map_err(|error| AppError::Upstream(error.to_string()))?,
            rpc_url: config.rpc_url.clone(),
            rpc_user: config.rpc_user.clone(),
            rpc_password: rpc_password.trim().to_owned(),
            network: config.network,
            sending_wallet: config.sending_wallet_name.clone(),
            receiving_wallet: config.receiving_wallet_name.clone(),
        };
        let info = client.chain_info().await?;
        let expected_chain = match config.network {
            Network::Bitcoin => "main",
            Network::Regtest => "regtest",
            _ => unreachable!("configuration validation limits Bitcoin networks"),
        };
        if info.chain != expected_chain {
            return Err(AppError::Config(format!(
                "Bitcoin Core is on '{}' but the configured network is '{expected_chain}'",
                info.chain
            )));
        }
        client.ensure_wallets_loaded().await?;
        Ok(client)
    }

    pub async fn chain_info(&self) -> AppResult<ChainInfo> {
        Ok(self.call(None, "getblockchaininfo", json!([])).await?)
    }

    async fn ensure_wallets_loaded(&self) -> AppResult<()> {
        let loaded: Vec<String> = self.call(None, "listwallets", json!([])).await?;
        for wallet in [&self.sending_wallet, &self.receiving_wallet] {
            if loaded.contains(wallet) {
                continue;
            }
            tracing::info!(wallet, "loading Bitcoin Core wallet");
            self.call::<Value>(None, "loadwallet", json!([wallet]))
                .await
                .map_err(|failure| {
                    AppError::Config(format!(
                        "could not load Bitcoin Core wallet '{wallet}': {}",
                        AppError::from(failure)
                    ))
                })?;
        }
        Ok(())
    }

    fn wallet_name(&self, wallet: Wallet) -> &str {
        match wallet {
            Wallet::Sending => &self.sending_wallet,
            Wallet::Receiving => &self.receiving_wallet,
        }
    }

    pub async fn new_receiving_address(&self) -> AppResult<String> {
        Ok(self
            .call(
                Some(&self.receiving_wallet),
                "getnewaddress",
                json!([ADDRESS_LABEL, ADDRESS_TYPE]),
            )
            .await?)
    }

    /// Builds and signs an `OP_RETURN` transaction. The fee is set from the
    /// signed transaction's virtual size, so the fee rate is exact.
    pub async fn create_signed_op_return(
        &self,
        message: &[u8],
        fee_rate_sat_vb: u64,
        funding: &Funding,
    ) -> AppResult<SignedOpReturn> {
        let op_return = op_return_script(message)?;
        let wallet = match funding {
            Funding::SendingWallet => Wallet::Sending,
            Funding::PaymentOutput { .. } => Wallet::Receiving,
        };
        let change_script = self.change_script(wallet).await?;
        let (outpoint, input_sats) = match funding {
            Funding::SendingWallet => {
                let probe = build_op_return_transaction(
                    OutPoint::null(),
                    u64::MAX,
                    0,
                    change_script.clone(),
                    op_return.clone(),
                )?;
                let estimated_fee = fee_for(fee_rate_sat_vb, estimated_vsize(&probe))?;
                self.select_sending_output(estimated_fee.saturating_add(DUST_SATS))
                    .await?
            }
            Funding::PaymentOutput {
                txid,
                vout,
                amount_sats,
            } => (
                OutPoint {
                    txid: parse_txid(txid)?,
                    vout: *vout,
                },
                *amount_sats,
            ),
        };
        let unsigned = build_op_return_transaction(
            outpoint,
            input_sats,
            0,
            change_script.clone(),
            op_return.clone(),
        )?;
        let mut fee_sats = fee_for(fee_rate_sat_vb, estimated_vsize(&unsigned))?;
        let mut signed = self
            .sign(wallet, &with_fee(&unsigned, input_sats, fee_sats)?)
            .await?;
        let exact_fee = fee_for(fee_rate_sat_vb, signed.vsize())?;
        if exact_fee != fee_sats {
            fee_sats = exact_fee;
            signed = self
                .sign(wallet, &with_fee(&unsigned, input_sats, fee_sats)?)
                .await?;
        }
        Ok(SignedOpReturn {
            transaction: signed,
            fee_sats,
            wallet,
        })
    }

    async fn change_script(&self, wallet: Wallet) -> AppResult<ScriptBuf> {
        let address_text: String = self
            .call(
                Some(self.wallet_name(wallet)),
                "getrawchangeaddress",
                json!([ADDRESS_TYPE]),
            )
            .await?;
        let address = Address::from_str(&address_text)
            .map_err(|error| {
                AppError::Upstream(format!("Bitcoin Core returned an invalid address: {error}"))
            })?
            .require_network(self.network)
            .map_err(|error| {
                AppError::Upstream(format!(
                    "Bitcoin Core returned an address on the wrong network: {error}"
                ))
            })?;
        Ok(address.script_pubkey())
    }

    async fn select_sending_output(&self, required_sats: u64) -> AppResult<(OutPoint, u64)> {
        let mut unspent: Vec<UnspentOutput> = self
            .call(
                Some(&self.sending_wallet),
                "listunspent",
                json!([0, 9_999_999]),
            )
            .await?;
        // Prefer the most confirmed output, like the Scala service, and
        // fall back to unconfirmed outputs.
        unspent.sort_by_key(|output| Reverse(output.confirmations));
        let (output, amount) = unspent
            .into_iter()
            .filter(|output| output.spendable)
            .find_map(|output| {
                let amount = number_to_sats(&output.amount).ok()?;
                (amount >= required_sats).then_some((output, amount))
            })
            .ok_or_else(|| {
                AppError::Upstream(format!(
                    "sending wallet has no spendable output worth at least {required_sats} sats"
                ))
            })?;
        Ok((
            OutPoint {
                txid: parse_txid(&output.txid)?,
                vout: output.vout,
            },
            amount,
        ))
    }

    async fn sign(&self, wallet: Wallet, unsigned: &Transaction) -> AppResult<Transaction> {
        let raw = consensus::encode::serialize_hex(unsigned);
        let signed: SignedTransaction = self
            .call(
                Some(self.wallet_name(wallet)),
                "signrawtransactionwithwallet",
                json!([raw]),
            )
            .await?;
        if !signed.complete {
            return Err(AppError::Upstream(format!(
                "Bitcoin Core did not fully sign the transaction: {:?}",
                signed.errors
            )));
        }
        let bytes = hex::decode(signed.hex).map_err(|error| {
            AppError::Upstream(format!(
                "Bitcoin Core returned invalid transaction hex: {error}"
            ))
        })?;
        Ok(consensus::deserialize(&bytes)?)
    }

    pub async fn broadcast(&self, transaction: &Transaction) -> AppResult<Txid> {
        let raw = consensus::encode::serialize_hex(transaction);
        let txid: String = match self.call(None, "sendrawtransaction", json!([raw, 0])).await {
            Ok(txid) => txid,
            Err(RpcFailure::Rpc { code, message }) if transaction_is_known(code, &message) => {
                return Ok(transaction.compute_txid());
            }
            Err(failure) => return Err(failure.into()),
        };
        parse_txid(&txid)
    }

    /// Locks an output in the wallet so that later transactions do not spend
    /// it. Use this after a broadcast that Bitcoin Core did not accept.
    pub async fn lock_output(&self, wallet: Wallet, outpoint: OutPoint) -> AppResult<()> {
        let name = self.wallet_name(wallet);
        let output = json!([{ "txid": outpoint.txid.to_string(), "vout": outpoint.vout }]);
        let persistent = self
            .call::<bool>(Some(name), "lockunspent", json!([false, output, true]))
            .await;
        if persistent.is_ok() {
            return Ok(());
        }
        // Bitcoin Core before 23.0 has no persistent locks.
        self.call::<bool>(Some(name), "lockunspent", json!([false, output]))
            .await?;
        Ok(())
    }

    /// Lists the payments in one transaction to labelled receiving
    /// addresses. Transactions that the receiving wallet does not know
    /// produce an empty list.
    pub async fn received_in_transaction(&self, txid: &str) -> AppResult<Vec<ReceivedPayment>> {
        let transaction: WalletTransaction = match self
            .call(
                Some(&self.receiving_wallet),
                "gettransaction",
                json!([txid, true]),
            )
            .await
        {
            Ok(transaction) => transaction,
            Err(RpcFailure::Rpc { code, .. }) if code == RPC_INVALID_ADDRESS_OR_KEY => {
                return Ok(Vec::new());
            }
            Err(failure) => return Err(failure.into()),
        };
        transaction
            .details
            .into_iter()
            .filter(|detail| detail.category == "receive")
            .filter_map(|detail| match (detail.address, detail.vout) {
                (Some(address), Some(vout)) => Some((address, vout, detail.amount)),
                _ => None,
            })
            .map(|(address, vout, amount)| {
                Ok(ReceivedPayment {
                    address,
                    amount_sats: number_to_sats(&amount)?,
                    txid: transaction.txid.clone(),
                    vout,
                    confirmations: transaction.confirmations,
                })
            })
            .collect()
    }

    /// The value of one output of a transaction that `wallet` knows. Used
    /// to recover the fee of a stored transaction from its input.
    pub async fn output_value(&self, wallet: Wallet, outpoint: OutPoint) -> AppResult<u64> {
        let transaction: WalletTransaction = self
            .call(
                Some(self.wallet_name(wallet)),
                "gettransaction",
                json!([outpoint.txid.to_string(), true]),
            )
            .await?;
        let bytes = hex::decode(&transaction.hex).map_err(|error| {
            AppError::Upstream(format!(
                "Bitcoin Core returned invalid transaction hex: {error}"
            ))
        })?;
        let parsed: Transaction = consensus::deserialize(&bytes)?;
        usize::try_from(outpoint.vout)
            .ok()
            .and_then(|vout| parsed.output.get(vout))
            .map(|output| output.value.to_sat())
            .ok_or_else(|| {
                AppError::Upstream(format!("the wallet transaction has no output {outpoint}"))
            })
    }

    /// Finds the payment output for one address in one transaction.
    pub async fn payment_output(
        &self,
        txid: &str,
        address: &str,
    ) -> AppResult<Option<ReceivedPayment>> {
        Ok(self
            .received_in_transaction(txid)
            .await?
            .into_iter()
            .find(|payment| payment.address == address))
    }

    /// Lists recent payments to labelled receiving addresses with one wallet
    /// call. Conflicted transactions are excluded.
    pub async fn received_payments(&self) -> AppResult<Vec<ReceivedPayment>> {
        let rows: Vec<ListedTransaction> = self
            .call(
                Some(&self.receiving_wallet),
                "listtransactions",
                json!([ADDRESS_LABEL, LISTED_TRANSACTIONS, 0, true]),
            )
            .await?;
        rows.into_iter()
            .filter(|row| row.category == "receive" && row.confirmations >= 0)
            .filter_map(|row| match (row.address, row.vout, row.txid) {
                (Some(address), Some(vout), Some(txid)) => {
                    Some((address, vout, txid, row.amount, row.confirmations))
                }
                _ => None,
            })
            .map(|(address, vout, txid, amount, confirmations)| {
                Ok(ReceivedPayment {
                    address,
                    amount_sats: number_to_sats(&amount)?,
                    txid,
                    vout,
                    confirmations,
                })
            })
            .collect()
    }

    async fn call<T: DeserializeOwned>(
        &self,
        wallet: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<T, RpcFailure> {
        let mut url = self.rpc_url.clone();
        if let Some(wallet) = wallet {
            url.path_segments_mut()
                .map_err(|()| {
                    RpcFailure::Other(AppError::Config(
                        "bitcoin.rpc_url cannot be a base URL".to_owned(),
                    ))
                })?
                .extend(["wallet", wallet]);
        }
        let response = self
            .http
            .post(url)
            .basic_auth(&self.rpc_user, Some(&self.rpc_password))
            .json(&json!({"jsonrpc": "2.0", "id": "op-return-bot", "method": method, "params": params}))
            .send()
            .await
            .map_err(|error| {
                RpcFailure::Other(AppError::Upstream(format!(
                    "Bitcoin RPC {method} failed: {error}"
                )))
            })?;
        let status = response.status();
        let envelope: RpcEnvelope<T> = response.json().await.map_err(|error| {
            RpcFailure::Other(AppError::Upstream(format!(
                "Bitcoin RPC {method} returned invalid JSON: {error}"
            )))
        })?;
        if let Some(error) = envelope.error {
            return Err(RpcFailure::Rpc {
                code: error.code,
                message: format!("Bitcoin RPC {method} {}", error.message),
            });
        }
        envelope.result.ok_or_else(|| {
            RpcFailure::Other(AppError::Upstream(format!(
                "Bitcoin RPC {method} returned no result (HTTP {status})"
            )))
        })
    }
}

fn parse_txid(txid: &str) -> AppResult<Txid> {
    Txid::from_str(txid).map_err(|error| {
        AppError::Upstream(format!("Bitcoin Core returned an invalid txid: {error}"))
    })
}

fn transaction_is_known(code: i64, message: &str) -> bool {
    code == -27
        || message.contains("txn-already-known")
        || message.contains("txn-already-in-mempool")
        || message.contains("already in block chain")
}

/// Returns true when Bitcoin Core rejected a transaction because an input
/// no longer exists.
#[must_use]
pub fn is_missing_inputs_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("missing inputs")
        || message.contains("missing-inputs")
        || message.contains("missingorspent")
}

/// Returns true when Bitcoin Core rejected a transaction only because its
/// `OP_RETURN` output or its size breaks the node's standardness policy.
/// MARA Slipstream accepts such transactions.
#[must_use]
pub fn is_policy_rejection(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    ["scriptpubkey", "datacarrier", "tx-size", "multi-op-return"]
        .iter()
        .any(|reason| message.contains(reason))
}

fn op_return_script(message: &[u8]) -> AppResult<ScriptBuf> {
    let push_bytes = PushBytesBuf::try_from(message.to_vec())
        .map_err(|error| AppError::InvalidRequest(format!("message cannot be encoded: {error}")))?;
    Ok(Builder::new()
        .push_opcode(opcodes::all::OP_RETURN)
        .push_slice(push_bytes)
        .into_script())
}

/// Estimates the virtual size of an unsigned single-input transaction.
fn estimated_vsize(unsigned: &Transaction) -> usize {
    (unsigned.base_size() * 4 + WITNESS_WEIGHT_ESTIMATE).div_ceil(4)
}

fn fee_for(fee_rate_sat_vb: u64, vsize: usize) -> AppResult<u64> {
    let vsize = u64::try_from(vsize)
        .map_err(|_| AppError::Internal("transaction size overflowed".to_owned()))?;
    fee_rate_sat_vb
        .checked_mul(vsize)
        .ok_or_else(|| AppError::Internal("transaction fee overflowed".to_owned()))
}

fn with_fee(unsigned: &Transaction, input_sats: u64, fee_sats: u64) -> AppResult<Transaction> {
    let change_sats = input_sats.checked_sub(fee_sats).ok_or_else(|| {
        AppError::Internal("selected output cannot pay the transaction fee".to_owned())
    })?;
    if change_sats < DUST_SATS {
        return Err(AppError::Internal(format!(
            "change output would be dust: input {input_sats} sats, fee {fee_sats} sats"
        )));
    }
    let mut transaction = unsigned.clone();
    transaction.output[0].value = Amount::from_sat(change_sats);
    Ok(transaction)
}

fn build_op_return_transaction(
    outpoint: OutPoint,
    input_sats: u64,
    fee_sats: u64,
    change_script: ScriptBuf,
    op_return: ScriptBuf,
) -> AppResult<Transaction> {
    let change_sats = input_sats.checked_sub(fee_sats).ok_or_else(|| {
        AppError::Internal("selected output cannot pay the transaction fee".to_owned())
    })?;
    Ok(Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::from_consensus(106),
        input: vec![TxIn {
            previous_output: outpoint,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![
            TxOut {
                value: Amount::from_sat(change_sats),
                script_pubkey: change_script,
            },
            TxOut {
                value: Amount::ZERO,
                script_pubkey: op_return,
            },
        ],
    })
}

fn number_to_sats(number: &serde_json::Number) -> AppResult<u64> {
    Amount::from_str_in(&number.to_string(), bitcoin::Denomination::Bitcoin)
        .map(Amount::to_sat)
        .map_err(|error| {
            AppError::Upstream(format!("Bitcoin Core returned an invalid amount: {error}"))
        })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{Json, Router, extract::Path, extract::State, routing::post};
    use bitcoin::{WPubkeyHash, hashes::Hash};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::*;

    const PAYMENT_TXID: &str = "0000000000000000000000000000000000000000000000000000000000000002";

    #[derive(Clone)]
    struct RpcState {
        address: Arc<String>,
        calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    fn fake_sign(hex_tx: &str) -> String {
        let bytes = hex::decode(hex_tx).unwrap();
        let mut transaction: Transaction = consensus::deserialize(&bytes).unwrap();
        for input in &mut transaction.input {
            input.witness = Witness::from_slice(&[vec![1_u8; 72], vec![2_u8; 33]]);
        }
        consensus::encode::serialize_hex(&transaction)
    }

    fn fake_result(state: &RpcState, method: &str, params: &Value) -> Option<Value> {
        let result = match method {
            "getblockchaininfo" => json!({ "blocks": 200, "chain": "regtest" }),
            "listwallets" => json!(["sending"]),
            "loadwallet" => json!({ "name": params[0] }),
            "getrawchangeaddress" => json!(state.address.as_str()),
            "listunspent" => json!([{
                "txid": "0000000000000000000000000000000000000000000000000000000000000001",
                "vout": 0,
                "amount": 0.001,
                "confirmations": 6,
                "spendable": true,
                "safe": true
            }]),
            "signrawtransactionwithwallet" => json!({
                "hex": fake_sign(params[0].as_str().unwrap()),
                "complete": true,
                "errors": []
            }),
            "lockunspent" => json!(true),
            "gettransaction" => json!({
                "txid": PAYMENT_TXID,
                "confirmations": 0,
                "details": [{
                    "address": state.address.as_str(),
                    "category": "receive",
                    "amount": 0.0002,
                    "vout": 1
                }]
            }),
            "listtransactions" => json!([{
                "address": state.address.as_str(),
                "category": "receive",
                "amount": 0.0002,
                "vout": 1,
                "txid": PAYMENT_TXID,
                "confirmations": 0
            }]),
            _ => return None,
        };
        Some(result)
    }

    async fn fake_rpc(
        State(state): State<RpcState>,
        wallet: Option<Path<String>>,
        Json(request): Json<Value>,
    ) -> Json<Value> {
        let method = request["method"].as_str().unwrap_or_default().to_owned();
        let wallet = wallet.map(|Path(name)| name).unwrap_or_default();
        state.calls.lock().unwrap().push((wallet, method.clone()));
        match fake_result(&state, &method, &request["params"]) {
            Some(result) => Json(json!({ "result": result, "error": null })),
            None => Json(
                json!({ "result": null, "error": { "code": -32601, "message": "unknown method" } }),
            ),
        }
    }

    async fn fake_client() -> (TempDir, BitcoinClient, RpcState) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = Address::from_script(
            &ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([2; 20])),
            Network::Regtest,
        )
        .unwrap()
        .to_string();
        let state = RpcState {
            address: Arc::new(address),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route("/", post(fake_rpc))
            .route("/wallet/{*wallet}", post(fake_rpc))
            .with_state(state.clone());
        let socket = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let directory = tempfile::tempdir().unwrap();
        let password = directory.path().join("rpc-password");
        std::fs::write(&password, "secret\n").unwrap();
        let client = BitcoinClient::connect(&BitcoinConfig {
            network: Network::Regtest,
            rpc_url: Url::parse(&format!("http://{socket}")).unwrap(),
            rpc_user: "test".to_owned(),
            rpc_password_file: password,
            sending_wallet_name: "sending".to_owned(),
            receiving_wallet_name: "receiving".to_owned(),
            wallet_notify_key_file: directory.path().join("wallet-key"),
        })
        .await
        .unwrap();
        (directory, client, state)
    }

    fn p2wpkh_change() -> ScriptBuf {
        ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([0x42; 20]))
    }

    fn outpoint(vout: u32) -> OutPoint {
        OutPoint {
            txid: Txid::from_str(
                "0000000000000000000000000000000000000000000000000000000000000001",
            )
            .unwrap(),
            vout,
        }
    }

    #[test]
    fn builds_the_legacy_transaction_shape() {
        let transaction = build_op_return_transaction(
            outpoint(2),
            20_000,
            2_000,
            p2wpkh_change(),
            op_return_script(b"hello").unwrap(),
        )
        .unwrap();
        assert_eq!(transaction.version, transaction::Version::TWO);
        assert_eq!(transaction.lock_time.to_consensus_u32(), 106);
        assert_eq!(transaction.input[0].sequence, Sequence::MAX);
        assert_eq!(transaction.output[0].value.to_sat(), 18_000);
        assert_eq!(transaction.output[1].value, Amount::ZERO);
        assert!(transaction.output[1].script_pubkey.is_op_return());
    }

    #[test]
    fn estimates_the_signed_size_for_any_message_length() {
        for length in [1_usize, 80, 81, 300, 1_000, 99_000] {
            let message = vec![0x2a; length];
            let unsigned = build_op_return_transaction(
                outpoint(0),
                1_000_000,
                0,
                p2wpkh_change(),
                op_return_script(&message).unwrap(),
            )
            .unwrap();
            let mut signed = unsigned.clone();
            signed.input[0].witness = Witness::from_slice(&[vec![1_u8; 72], vec![2_u8; 33]]);
            assert_eq!(
                estimated_vsize(&unsigned),
                signed.vsize(),
                "message length {length}"
            );
            assert!(signed.vsize() > length, "the size must include the message");
        }
    }

    #[test]
    fn rejects_dust_change() {
        let unsigned = build_op_return_transaction(
            outpoint(0),
            1_000,
            0,
            p2wpkh_change(),
            op_return_script(b"hello").unwrap(),
        )
        .unwrap();
        assert!(with_fee(&unsigned, 1_000, 700).is_err());
        assert_eq!(
            with_fee(&unsigned, 1_000, 600).unwrap().output[0]
                .value
                .to_sat(),
            400
        );
    }

    #[test]
    fn converts_exact_bitcoin_amounts() {
        assert_eq!(
            number_to_sats(&serde_json::Number::from_str("0.00001234").unwrap()).unwrap(),
            1_234
        );
    }

    #[test]
    fn recognizes_safe_broadcast_retries() {
        assert!(transaction_is_known(
            -27,
            "Bitcoin RPC sendrawtransaction Transaction already in block chain"
        ));
        assert!(transaction_is_known(-26, "txn-already-known"));
        assert!(!transaction_is_known(
            -26,
            "mandatory-script-verify-flag-failed"
        ));
    }

    #[test]
    fn recognizes_standardness_rejections_only() {
        assert!(is_policy_rejection("error -26: scriptpubkey"));
        assert!(is_policy_rejection("error -26: tx-size"));
        assert!(!is_policy_rejection(
            "error -26: too-long-mempool-chain, too many unconfirmed ancestors [limit: 25]"
        ));
        assert!(!is_policy_rejection("error -26: min relay fee not met"));
        assert!(!is_policy_rejection(
            "error -25: bad-txns-inputs-missingorspent"
        ));
    }

    #[test]
    fn recognizes_missing_inputs() {
        assert!(is_missing_inputs_error(
            "error -25: bad-txns-inputs-missingorspent"
        ));
        assert!(is_missing_inputs_error("Missing inputs"));
        assert!(!is_missing_inputs_error("insufficient fee"));
    }

    #[tokio::test]
    #[ignore = "requires local TCP sockets, which the Nix build sandbox blocks"]
    async fn creates_a_transaction_through_core_rpc() {
        let (_directory, client, state) = fake_client().await;
        let signed = client
            .create_signed_op_return(b"hello", 10, &Funding::SendingWallet)
            .await
            .unwrap();
        assert_eq!(signed.transaction.vsize(), 126);
        assert_eq!(signed.fee_sats, 1_260);
        assert_eq!(signed.wallet, Wallet::Sending);
        assert_eq!(signed.transaction.output.len(), 2);
        assert!(signed.transaction.output[1].script_pubkey.is_op_return());
        let calls = state.calls.lock().unwrap();
        assert!(calls.contains(&(String::new(), "loadwallet".to_owned())));
        assert!(calls.contains(&(
            "sending".to_owned(),
            "signrawtransactionwithwallet".to_owned()
        )));
    }

    #[tokio::test]
    #[ignore = "requires local TCP sockets, which the Nix build sandbox blocks"]
    async fn prices_large_messages_by_virtual_size() {
        let (_directory, client, _state) = fake_client().await;
        let message = vec![0x2a; 1_000];
        let signed = client
            .create_signed_op_return(&message, 5, &Funding::SendingWallet)
            .await
            .unwrap();
        assert_eq!(signed.transaction.vsize(), 1_125);
        assert_eq!(signed.fee_sats, 5 * 1_125);
    }

    #[tokio::test]
    #[ignore = "requires local TCP sockets, which the Nix build sandbox blocks"]
    async fn spends_the_payment_output_from_the_receiving_wallet() {
        let (_directory, client, state) = fake_client().await;
        let payment = client
            .payment_output(PAYMENT_TXID, state.address.as_str())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(payment.vout, 1);
        assert_eq!(payment.amount_sats, 20_000);
        let signed = client
            .create_signed_op_return(
                b"hello",
                10,
                &Funding::PaymentOutput {
                    txid: payment.txid.clone(),
                    vout: payment.vout,
                    amount_sats: payment.amount_sats,
                },
            )
            .await
            .unwrap();
        assert_eq!(signed.wallet, Wallet::Receiving);
        assert_eq!(
            signed.transaction.input[0].previous_output,
            OutPoint {
                txid: Txid::from_str(PAYMENT_TXID).unwrap(),
                vout: 1
            }
        );
        assert_eq!(
            signed.transaction.output[0].value.to_sat(),
            20_000 - signed.fee_sats
        );
        let calls = state.calls.lock().unwrap();
        assert!(calls.contains(&(
            "receiving".to_owned(),
            "signrawtransactionwithwallet".to_owned()
        )));
        assert!(!calls.iter().any(|(_, method)| method == "listunspent"));
    }
}
