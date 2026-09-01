use serde::{Deserialize, Serialize};

use crate::{AppError, AppResult};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpReturnRequest {
    pub id: i64,
    pub message: Vec<u8>,
    pub no_twitter: bool,
    pub fee_rate_sat_vb: u64,
    pub node_id: Option<String>,
    pub telegram_id: Option<i64>,
    pub nostr_key: Option<String>,
    pub created_at: i64,
    pub transaction: Option<String>,
    pub txid: Option<String>,
    pub profit_sats: Option<i64>,
    pub chain_fee_sats: Option<i64>,
    pub vsize: Option<i64>,
    pub closed: bool,
    pub btc_price_cents: i64,
}

impl OpReturnRequest {
    #[must_use]
    pub fn message_text(&self) -> String {
        String::from_utf8_lossy(&self.message).into_owned()
    }

    #[must_use]
    pub const fn payment_status(&self, paid: bool, on_chain_txid: Option<&str>) -> PaymentStatus {
        if self.txid.is_some() {
            PaymentStatus::Complete
        } else if paid || on_chain_txid.is_some() {
            PaymentStatus::Pending
        } else {
            PaymentStatus::Unpaid
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invoice {
    pub payment_hash: String,
    pub request_id: i64,
    pub bolt11: String,
    pub paid: bool,
    pub amount_sats: Option<i64>,
    pub lightning_backend: LightningBackend,
    pub claim_preimage: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnChainPayment {
    pub address: String,
    pub request_id: i64,
    pub expected_amount_sats: i64,
    pub amount_paid_sats: Option<i64>,
    pub txid: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LightningBackend {
    Lnd,
    LdkServer,
}

impl LightningBackend {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lnd => "lnd",
            Self::LdkServer => "ldk-server",
        }
    }
}

impl TryFrom<&str> for LightningBackend {
    type Error = AppError;

    fn try_from(value: &str) -> AppResult<Self> {
        match value {
            "lnd" => Ok(Self::Lnd),
            "ldk-server" => Ok(Self::LdkServer),
            other => Err(AppError::Internal(format!(
                "database contains unknown Lightning backend '{other}'"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PaymentStatus {
    Unpaid,
    Pending,
    Complete,
}

pub(crate) fn decode_legacy_bytes(value: &[u8]) -> AppResult<Vec<u8>> {
    if let Ok(text) = std::str::from_utf8(value)
        && text.len().is_multiple_of(2)
        && text.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return hex::decode(text).map_err(|error| {
            AppError::Internal(format!("database contains invalid hex bytes: {error}"))
        });
    }
    Ok(value.to_vec())
}

pub(crate) fn encode_legacy_bytes(value: &[u8]) -> String {
    hex::encode(value)
}

pub(crate) fn decode_legacy_fee_rate(value: &str) -> AppResult<u64> {
    let bytes = hex::decode(value).map_err(|error| {
        AppError::Internal(format!("database contains an invalid fee rate: {error}"))
    })?;
    let bytes: [u8; 8] = bytes
        .try_into()
        .map_err(|_| AppError::Internal("database fee rate must contain eight bytes".to_owned()))?;
    Ok(u64::from_le_bytes(bytes))
}

pub(crate) fn encode_legacy_fee_rate(value: u64) -> String {
    hex::encode(value.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_legacy_hex_message() {
        assert_eq!(decode_legacy_bytes(b"68656c6c6f").unwrap(), b"hello");
    }

    #[test]
    fn preserves_native_blob() {
        assert_eq!(decode_legacy_bytes(&[0, 255, 42]).unwrap(), [0, 255, 42]);
    }

    #[test]
    fn round_trips_legacy_fee_rate() {
        let encoded = encode_legacy_fee_rate(75);
        assert_eq!(encoded, "4b00000000000000");
        assert_eq!(decode_legacy_fee_rate(&encoded).unwrap(), 75);
    }
}
