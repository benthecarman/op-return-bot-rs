use crate::{AppError, AppResult, config::PaymentConfig};

pub const STANDARD_OP_RETURN_BYTES: usize = 80;
const ESTIMATED_BASE_VBYTES: u64 = 125;
const PRE_HALVING_HEIGHT: u64 = 1_049_999;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PriceQuote {
    pub amount_sats: u64,
    pub fee_rate_sat_vb: u64,
    pub non_standard: bool,
}

pub fn quote(
    config: &PaymentConfig,
    message: &[u8],
    no_twitter: bool,
    fastest_fee_sat_vb: u64,
    block_height: u64,
) -> AppResult<PriceQuote> {
    if message.is_empty() {
        return Err(AppError::InvalidRequest(
            "message must not be empty".to_owned(),
        ));
    }
    if message.len() > config.message_max_bytes {
        return Err(AppError::InvalidRequest(format!(
            "message is too long; the maximum is {} UTF-8 bytes",
            config.message_max_bytes
        )));
    }

    let non_standard = message.len() > STANDARD_OP_RETURN_BYTES;
    let mut fee_rate = if non_standard {
        fastest_fee_sat_vb.saturating_mul(2).max(5)
    } else {
        fastest_fee_sat_vb.saturating_add(4)
    };
    if block_height == PRE_HALVING_HEIGHT {
        fee_rate = fee_rate.saturating_mul(10);
    }

    let message_size = u64::try_from(message.len())
        .map_err(|_| AppError::InvalidRequest("message size is not supported".to_owned()))?;
    let estimated_chain_fee = fee_rate
        .checked_mul(2)
        .and_then(|value| value.checked_mul(ESTIMATED_BASE_VBYTES + message_size))
        .ok_or_else(|| AppError::InvalidRequest("price calculation overflowed".to_owned()))?;
    let amount_sats = estimated_chain_fee
        .checked_add(config.application_fee_sats)
        .and_then(|value| {
            value.checked_add(if no_twitter {
                config.private_fee_sats
            } else {
                0
            })
        })
        .and_then(|value| {
            value.checked_add(if non_standard {
                config.non_standard_fee_sats
            } else {
                0
            })
        })
        .ok_or_else(|| AppError::InvalidRequest("price calculation overflowed".to_owned()))?;

    Ok(PriceQuote {
        amount_sats,
        fee_rate_sat_vb: fee_rate,
        non_standard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_legacy_standard_formula() {
        let result = quote(&PaymentConfig::default(), b"hello", false, 10, 900_000).unwrap();
        assert_eq!(result.fee_rate_sat_vb, 14);
        assert_eq!(result.amount_sats, 4_977);
        assert!(!result.non_standard);
    }

    #[test]
    fn applies_non_standard_and_private_fees() {
        let message = vec![42; 81];
        let result = quote(&PaymentConfig::default(), &message, true, 2, 900_000).unwrap();
        assert_eq!(result.fee_rate_sat_vb, 5);
        assert_eq!(result.amount_sats, 5_397);
        assert!(result.non_standard);
    }

    #[test]
    fn uses_utf8_bytes_for_the_limit() {
        let config = PaymentConfig {
            message_max_bytes: 3,
            ..PaymentConfig::default()
        };
        assert!(quote(&config, "🦀".as_bytes(), false, 1, 1).is_err());
    }
}
