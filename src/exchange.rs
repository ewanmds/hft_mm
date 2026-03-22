use anyhow::{anyhow, Result};
use k256::ecdsa::SigningKey;
use sha3::{Digest, Keccak256};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;

/// Keccak256 hash helper
fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

fn left_pad_32(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let start = 32 - bytes.len();
    out[start..].copy_from_slice(bytes);
    out
}

fn hash_string(value: &str) -> [u8; 32] {
    keccak256(value.as_bytes())
}

fn encode_u64_word(value: u64) -> [u8; 32] {
    left_pad_32(&value.to_be_bytes())
}

fn float_to_wire(value: f64) -> String {
    let mut s = format!("{:.8}", value);
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    if s == "-0" {
        "0".to_string()
    } else {
        s
    }
}

/// Hyperliquid exchange client
pub struct HyperLiquidExchange {
    client: Client,
    signing_key: SigningKey,
    account_address: String,
    info_url: String,
    exchange_url: String,
    symbol: String,
    perp_dex: String,
    asset_index: u32,
    is_mainnet: bool,
    vault_address: Option<String>,
}

// ── API Response Types ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OrderStatus {
    pub resting: Option<RestingOrder>,
    pub error: Option<String>,
    pub filled: Option<FilledOrder>,
}

#[derive(Debug, Deserialize)]
pub struct RestingOrder {
    pub oid: u64,
}

#[derive(Debug, Deserialize)]
pub struct FilledOrder {
    #[serde(rename = "totalSz")]
    pub total_sz: String,
    #[serde(rename = "avgPx")]
    pub avg_px: String,
    pub oid: u64,
}

#[derive(Debug, Deserialize)]
pub struct L2Snapshot {
    pub levels: Vec<Vec<L2Level>>,
}

#[derive(Debug, Deserialize)]
pub struct L2Level {
    pub px: String,
    pub sz: String,
    pub n: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserState {
    pub asset_positions: Option<Vec<AssetPositionEntry>>,
    pub margin_summary: Option<MarginSummary>,
    pub cross_margin_summary: Option<MarginSummary>,
}

#[derive(Debug, Deserialize)]
pub struct AssetPositionEntry {
    pub position: AssetPosition,
}

#[derive(Debug, Deserialize)]
pub struct AssetPosition {
    pub coin: String,
    pub szi: String,
    #[serde(rename = "unrealizedPnl")]
    pub unrealized_pnl: String,
    #[serde(rename = "entryPx")]
    pub entry_px: Option<String>,
    #[serde(rename = "marginUsed")]
    pub margin_used: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginSummary {
    pub account_value: String,
    pub total_margin_used: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct OpenOrder {
    pub coin: String,
    pub oid: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ApiSignature {
    r: String,
    s: String,
    v: u8,
}

#[derive(Debug, Deserialize)]
struct MetaResponse {
    universe: Vec<MetaAsset>,
}

#[derive(Debug, Deserialize)]
struct MetaAsset {
    name: String,
}

#[derive(Debug, Deserialize)]
struct PerpDexEntry {
    name: String,
}

// ── Bulk order/cancel request ──────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct OrderRequest {
    pub coin: String,
    pub is_buy: bool,
    pub sz: f64,
    pub limit_px: f64,
    pub reduce_only: bool,
    pub tif: String, // "Alo", "Ioc", "Gtc"
}

#[derive(Debug, Clone, Serialize)]
struct LimitOrderType {
    tif: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum SignedOrderType {
    Limit(LimitOrderType),
}

#[derive(Debug, Clone, Serialize)]
struct SignedOrderRequest {
    #[serde(rename = "a")]
    asset: u32,
    #[serde(rename = "b")]
    is_buy: bool,
    #[serde(rename = "p")]
    limit_px: String,
    #[serde(rename = "s")]
    sz: String,
    #[serde(rename = "r")]
    reduce_only: bool,
    #[serde(rename = "t")]
    order_type: SignedOrderType,
}

#[derive(Debug, Clone, Serialize)]
struct SignedCancelRequest {
    #[serde(rename = "a")]
    asset: u32,
    #[serde(rename = "o")]
    oid: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BulkOrderAction {
    orders: Vec<SignedOrderRequest>,
    grouping: String,
}

#[derive(Debug, Clone, Serialize)]
struct BulkCancelAction {
    cancels: Vec<SignedCancelRequest>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
enum ExchangeAction {
    Order(BulkOrderAction),
    Cancel(BulkCancelAction),
}

impl HyperLiquidExchange {
    pub async fn new(config: &Config) -> Result<Self> {
        let key_hex = config.agent_private_key.trim_start_matches("0x");
        let key_bytes = hex::decode(key_hex)?;
        let signing_key = SigningKey::from_slice(&key_bytes)
            .map_err(|e| anyhow!("Invalid private key: {}", e))?;

        let client = Client::builder()
            .pool_max_idle_per_host(4)
            .tcp_keepalive(Some(std::time::Duration::from_secs(30)))
            .tcp_nodelay(true)
            .build()?;

        let info_url = format!("{}/info", config.base_url);
        let exchange_url = format!("{}/exchange", config.base_url);
        let perp_dex = config.perp_dex().to_string();
        let full_symbol = &config.token.symbol;
        let base_coin = config.base_coin();
        let asset_index = Self::resolve_asset_index(
            &client,
            &info_url,
            &perp_dex,
            full_symbol,
            base_coin,
        )
        .await?;

        Ok(Self {
            client,
            signing_key,
            account_address: config.account_address.clone(),
            info_url,
            exchange_url,
            symbol: config.token.symbol.to_string(),
            perp_dex,
            asset_index,
            is_mainnet: !config.base_url.contains("testnet"),
            vault_address: None,
        })
    }

    // ── Timestamp ──────────────────────────────────────────────────

    fn timestamp_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    // ── Signing (Hyperliquid L1 phantom agent) ─────────────────────

    fn eip712_domain_hash() -> [u8; 32] {
        let type_hash =
            hash_string("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
        let name_hash = hash_string("Exchange");
        let version_hash = hash_string("1");
        let chain_id = encode_u64_word(1337);
        let verifying_contract = [0u8; 32];

        let mut encoded = Vec::with_capacity(160);
        encoded.extend_from_slice(&type_hash);
        encoded.extend_from_slice(&name_hash);
        encoded.extend_from_slice(&version_hash);
        encoded.extend_from_slice(&chain_id);
        encoded.extend_from_slice(&verifying_contract);
        keccak256(&encoded)
    }

    fn agent_struct_hash(connection_id: &[u8; 32], is_mainnet: bool) -> [u8; 32] {
        let type_hash = hash_string("Agent(string source,bytes32 connectionId)");
        let source_hash = hash_string(if is_mainnet { "a" } else { "b" });

        let mut encoded = Vec::with_capacity(96);
        encoded.extend_from_slice(&type_hash);
        encoded.extend_from_slice(&source_hash);
        encoded.extend_from_slice(connection_id);
        keccak256(&encoded)
    }

    fn sign_l1_action(&self, connection_id: [u8; 32]) -> Result<ApiSignature> {
        let domain_hash = Self::eip712_domain_hash();
        let struct_hash = Self::agent_struct_hash(&connection_id, self.is_mainnet);

        let mut digest_input = Vec::with_capacity(66);
        digest_input.push(0x19);
        digest_input.push(0x01);
        digest_input.extend_from_slice(&domain_hash);
        digest_input.extend_from_slice(&struct_hash);
        let digest = keccak256(&digest_input);

        let (signature, recovery_id) = self
            .signing_key
            .sign_prehash_recoverable(&digest)
            .map_err(|e| anyhow!("Signing failed: {}", e))?;

        let sig_bytes = signature.to_bytes();
        Ok(ApiSignature {
            r: format!("0x{}", hex::encode(&sig_bytes[..32])),
            s: format!("0x{}", hex::encode(&sig_bytes[32..])),
            v: recovery_id.to_byte() + 27,
        })
    }

    // ── Info API (POST /info) ──────────────────────────────────────

    fn compact_body(body: &str) -> String {
        let compact = body.split_whitespace().collect::<Vec<_>>().join(" ");
        if compact.chars().count() <= 240 {
            compact
        } else {
            let short: String = compact.chars().take(240).collect();
            format!("{}...", short)
        }
    }

    async fn post_json(client: &Client, url: &str, body: &Value) -> Result<Value> {
        let response = client
            .post(url)
            .json(body)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;

        if !status.is_success() {
            return Err(anyhow!(
                "HTTP {} from {}: {}",
                status,
                url,
                Self::compact_body(&text)
            ));
        }

        serde_json::from_str(&text).map_err(|e| {
            anyhow!(
                "Invalid JSON from {}: {} | body: {}",
                url,
                e,
                Self::compact_body(&text)
            )
        })
    }

    async fn resolve_asset_index(
        client: &Client,
        info_url: &str,
        perp_dex: &str,
        full_symbol: &str,
        base_coin: &str,
    ) -> Result<u32> {
        let offset = if perp_dex.is_empty() {
            0
        } else {
            let resp = Self::post_json(client, info_url, &json!({ "type": "perpDexs" })).await?;
            let perp_dexs: Vec<Option<PerpDexEntry>> = serde_json::from_value(resp)?;
            let builder_index = perp_dexs
                .iter()
                .skip(1)
                .enumerate()
                .find_map(|(i, entry)| {
                    entry
                        .as_ref()
                        .filter(|dex| dex.name == perp_dex)
                        .map(|_| i as u32)
                })
                .ok_or_else(|| anyhow!("perp dex '{}' not found in perpDexs", perp_dex))?;
            110_000 + builder_index * 10_000
        };

        let meta_body = if perp_dex.is_empty() {
            json!({ "type": "meta" })
        } else {
            json!({ "type": "meta", "dex": perp_dex })
        };

        let resp = Self::post_json(client, info_url, &meta_body).await?;
        let meta = serde_json::from_value::<MetaResponse>(resp)?;

        meta.universe
            .iter()
            .enumerate()
            .find_map(|(idx, asset)| {
                let asset_base = asset.name.rsplit(':').next().unwrap_or(asset.name.as_str());
                if asset.name == full_symbol || asset.name == base_coin || asset_base == base_coin {
                    Some(offset + idx as u32)
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                anyhow!(
                    "asset {} not found in meta universe for dex '{}'",
                    full_symbol,
                    if perp_dex.is_empty() { "<default>" } else { perp_dex }
                )
            })
    }

    async fn info_request(&self, body: Value) -> Result<Value> {
        Self::post_json(&self.client, &self.info_url, &body).await
    }

    fn action_connection_id(&self, action: &ExchangeAction, nonce: u64) -> Result<[u8; 32]> {
        let mut bytes =
            rmp_serde::to_vec_named(action).map_err(|e| anyhow!("msgpack encode failed: {}", e))?;
        bytes.extend_from_slice(&nonce.to_be_bytes());
        if let Some(vault_address) = &self.vault_address {
            let addr = hex::decode(vault_address.trim_start_matches("0x"))?;
            if addr.len() != 20 {
                return Err(anyhow!("invalid vault address length"));
            }
            bytes.push(1);
            bytes.extend_from_slice(&addr);
        } else {
            bytes.push(0);
        }
        Ok(keccak256(&bytes))
    }

    /// Get L2 order book snapshot
    pub async fn l2_snapshot(&self) -> Result<L2Snapshot> {
        let body = json!({
            "type": "l2Book",
            "coin": self.symbol,
        });
        let resp = self.info_request(body).await?;
        let snapshot: L2Snapshot = serde_json::from_value(resp)?;
        Ok(snapshot)
    }

    /// Get user state (positions, margin)
    pub async fn user_state(&self) -> Result<Value> {
        let body = json!({
            "type": "clearinghouseState",
            "user": self.account_address,
        });
        self.info_request(body).await
    }

    /// Get user state for a specific perp dex
    pub async fn user_state_dex(&self) -> Result<Value> {
        let mut body = json!({
            "type": "clearinghouseState",
            "user": self.account_address,
        });
        if !self.perp_dex.is_empty() {
            body["dex"] = Value::String(self.perp_dex.clone());
        }
        self.info_request(body).await
    }

    /// Get open orders
    pub async fn open_orders(&self) -> Result<Vec<OpenOrder>> {
        let mut body = json!({
            "type": "openOrders",
            "user": self.account_address,
        });
        if !self.perp_dex.is_empty() {
            body["dex"] = Value::String(self.perp_dex.clone());
        }
        let resp = self.info_request(body).await?;
        let orders: Vec<OpenOrder> = serde_json::from_value(resp)?;
        Ok(orders)
    }

    // ── Exchange API (POST /exchange) ──────────────────────────────

    async fn exchange_request(&self, action: &ExchangeAction, nonce: u64) -> Result<Value> {
        let connection_id = self.action_connection_id(action, nonce)?;
        let signature = self.sign_l1_action(connection_id)?;
        let action = serde_json::to_value(action)?;

        let body = json!({
            "action": action,
            "nonce": nonce,
            "signature": signature,
            "vaultAddress": self.vault_address,
        });

        Self::post_json(&self.client, &self.exchange_url, &body).await
    }

    /// Place bulk orders — the hot path
    #[inline]
    pub async fn bulk_orders(&self, orders: &[OrderRequest]) -> Result<Value> {
        let nonce = Self::timestamp_ms();

        let order_specs: Vec<SignedOrderRequest> = orders
            .iter()
            .map(|o| SignedOrderRequest {
                asset: self.asset_index,
                is_buy: o.is_buy,
                limit_px: float_to_wire(o.limit_px),
                sz: float_to_wire(o.sz),
                reduce_only: o.reduce_only,
                order_type: SignedOrderType::Limit(LimitOrderType { tif: o.tif.clone() }),
            })
            .collect();

        let action = ExchangeAction::Order(BulkOrderAction {
            orders: order_specs,
            grouping: "na".to_string(),
        });

        self.exchange_request(&action, nonce).await
    }

    /// Bulk cancel orders
    pub async fn bulk_cancel(&self, cancels: &[(String, u64)]) -> Result<Value> {
        let nonce = Self::timestamp_ms();

        let cancel_specs: Vec<SignedCancelRequest> = cancels
            .iter()
            .map(|(_coin, oid)| SignedCancelRequest {
                asset: self.asset_index,
                oid: *oid,
            })
            .collect();

        let action = ExchangeAction::Cancel(BulkCancelAction {
            cancels: cancel_specs,
        });

        self.exchange_request(&action, nonce).await
    }

    /// Place a single reduce-only IOC order for closing
    pub async fn close_order(&self, coin: &str, is_buy: bool, size: f64, price: f64) -> Result<Value> {
        let orders = vec![OrderRequest {
            coin: coin.to_string(),
            is_buy,
            sz: size,
            limit_px: price,
            reduce_only: true,
            tif: "Ioc".to_string(),
        }];
        self.bulk_orders(&orders).await
    }

    /// Parse position from user state response
    pub fn parse_position(&self, state: &Value) -> (f64, f64, Option<f64>, f64) {
        let base = self.symbol.split(':').last().unwrap_or(&self.symbol);

        if let Some(positions) = state.get("assetPositions").and_then(|v| v.as_array()) {
            for entry in positions {
                let pos = &entry["position"];
                let coin = pos["coin"].as_str().unwrap_or("");

                if coin == self.symbol || coin == base || self.symbol.ends_with(coin) {
                    let szi: f64 = pos["szi"].as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0.0);
                    let pnl: f64 = pos["unrealizedPnl"].as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0.0);
                    let entry_px: Option<f64> = pos["entryPx"].as_str()
                        .and_then(|s| s.parse().ok())
                        .filter(|&v: &f64| v > 0.0);
                    let margin: f64 = pos["marginUsed"].as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0.0);

                    return (szi, pnl, entry_px, margin);
                }
            }
        }
        (0.0, 0.0, None, 0.0)
    }

    /// Parse equity from user state
    pub fn parse_equity(state: &Value) -> f64 {
        if let Some(ms) = state.get("marginSummary") {
            if let Some(v) = ms.get("accountValue").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok()) {
                if v > 0.0 { return v; }
            }
        }
        if let Some(ms) = state.get("crossMarginSummary") {
            if let Some(v) = ms.get("accountValue").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok()) {
                if v > 0.0 { return v; }
            }
        }
        0.0
    }

    /// Parse bulk order response into (buy_oids, sell_oids, margin_errors)
    pub fn parse_bulk_result(
        &self,
        result: &Value,
        requests: &[OrderRequest],
    ) -> (Vec<u64>, Vec<u64>, u32, Option<String>) {
        let mut buy_oids = Vec::new();
        let mut sell_oids = Vec::new();
        let mut margin_errs = 0u32;
        let mut first_error: Option<String> = None;

        if result.get("status").and_then(|v| v.as_str()) != Some("ok") {
            first_error = Some(Self::compact_body(&result.to_string()));
            return (buy_oids, sell_oids, margin_errs, first_error);
        }

        if let Some(statuses) = result
            .pointer("/response/data/statuses")
            .and_then(|v| v.as_array())
        {
            for (i, st) in statuses.iter().enumerate() {
                if let Some(resting) = st.get("resting") {
                    if let Some(oid) = resting.get("oid").and_then(|v| v.as_u64()) {
                        if i < requests.len() && requests[i].is_buy {
                            buy_oids.push(oid);
                        } else {
                            sell_oids.push(oid);
                        }
                    }
                } else if let Some(err) = st.get("error").and_then(|v| v.as_str()) {
                    let lower = err.to_lowercase();
                    if lower.contains("insufficient") && lower.contains("margin") {
                        margin_errs += 1;
                    }
                    if first_error.is_none() {
                        first_error = Some(err.to_string());
                    }
                } else if let Some(err) = st.as_str() {
                    if err != "success" && first_error.is_none() {
                        first_error = Some(err.to_string());
                    }
                } else if first_error.is_none() {
                    first_error = Some(Self::compact_body(&st.to_string()));
                }
            }
        } else {
            first_error = Some(Self::compact_body(&result.to_string()));
        }

        (buy_oids, sell_oids, margin_errs, first_error)
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn base_coin(&self) -> &str {
        self.symbol.split(':').last().unwrap_or(&self.symbol)
    }
}
