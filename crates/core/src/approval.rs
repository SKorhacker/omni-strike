//! Approval binding. An approval authorises EXACTLY one normalised payload: the hash
//! covers every economically relevant field, so any change invalidates it.
//! Single-use consumption is enforced transactionally in the persistence layer.

use crate::domain::{InstrumentKey, Side, TradingMode};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Normalised, hash-bound order payload.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedPayload {
    pub prepared_id: uuid::Uuid,
    pub account: String,
    pub mode: TradingMode,
    pub instrument: InstrumentKey,
    pub side: Side,
    /// Native contracts, as they would be sent.
    pub qty_native: Decimal,
    /// Worst price per native contract.
    pub limit_price_native: Decimal,
    pub max_fee: Decimal,
    pub currency: String,
    pub expires_at: DateTime<Utc>,
}

impl PreparedPayload {
    /// Decimals are normalised ("1.50" == "1.5") so equal values hash equally.
    fn canonical(&self) -> String {
        let n = |d: Decimal| d.normalize().to_string();
        // Explicit, ordered, versioned encoding; do not rely on serde field order.
        format!(
            "v1|{}|{}|{:?}|{}|{}|{}|{:?}|{}|{}|{}|{}|{}",
            self.prepared_id,
            self.account,
            self.mode,
            self.instrument.platform,
            self.instrument.network,
            self.instrument.native_id,
            self.side,
            n(self.qty_native),
            n(self.limit_price_native),
            n(self.max_fee),
            self.currency,
            self.expires_at.timestamp_millis(),
        )
    }

    pub fn hash(&self) -> String {
        hex::encode(Sha256::digest(self.canonical().as_bytes()))
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ApprovalError {
    HashMismatch,
    Expired,
    AlreadyConsumed,
    NotFound,
}

/// Pure pre-check; the authoritative check-and-consume is the DB's atomic UPDATE.
pub fn verify(
    payload: &PreparedPayload,
    approved_hash: &str,
    approval_expires_at: DateTime<Utc>,
    consumed: bool,
    now: DateTime<Utc>,
) -> Result<(), ApprovalError> {
    if consumed {
        return Err(ApprovalError::AlreadyConsumed);
    }
    if now >= approval_expires_at || now >= payload.expires_at {
        return Err(ApprovalError::Expired);
    }
    if payload.hash() != approved_hash {
        return Err(ApprovalError::HashMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::PlatformId;
    use chrono::Duration;
    use rust_decimal_macros::dec;

    fn p() -> PreparedPayload {
        PreparedPayload {
            prepared_id: uuid::Uuid::nil(),
            account: "acct".into(),
            mode: TradingMode::Simulation,
            instrument: InstrumentKey {
                platform: PlatformId::Derive,
                network: "derive-mainnet".into(),
                native_id: "BTC-X".into(),
            },
            side: Side::Buy,
            qty_native: dec!(1.50),
            limit_price_native: dec!(100),
            max_fee: dec!(1),
            currency: "USDC".into(),
            expires_at: Utc::now() + Duration::minutes(5),
        }
    }

    #[test]
    fn equal_values_hash_equal() {
        let a = p();
        let mut b = a.clone();
        b.qty_native = dec!(1.5);
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn any_change_invalidates() {
        let a = p();
        let h = a.hash();
        let now = Utc::now();
        let exp = now + Duration::minutes(1);
        assert_eq!(verify(&a, &h, exp, false, now), Ok(()));
        for mutate in [
            (|x: &mut PreparedPayload| x.qty_native = dec!(1.51)) as fn(&mut PreparedPayload),
            |x| x.limit_price_native = dec!(101),
            |x| x.side = Side::Sell,
            |x| x.instrument.network = "other".into(),
            |x| x.account = "other".into(),
            |x| x.max_fee = dec!(2),
            |x| x.mode = TradingMode::Live,
        ] {
            let mut b = a.clone();
            mutate(&mut b);
            assert_eq!(
                verify(&b, &h, exp, false, now),
                Err(ApprovalError::HashMismatch)
            );
        }
    }

    #[test]
    fn expired_and_reused_rejected() {
        let a = p();
        let h = a.hash();
        let now = Utc::now();
        assert_eq!(verify(&a, &h, now, false, now), Err(ApprovalError::Expired));
        assert_eq!(
            verify(&a, &h, now + Duration::minutes(1), true, now),
            Err(ApprovalError::AlreadyConsumed)
        );
    }
}
