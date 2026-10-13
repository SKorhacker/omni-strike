//! Per-user country filter: a venue is never offered to a user from a country its terms exclude.
//!
//! Omni Strike is non-custodial, but each venue's terms still apply to its users, and orders reach
//! the venue from OUR servers — routing a restricted user through us would be circumvention. So every
//! request may carry the user's country (ISO 3166-1 alpha-2, declared by the integrator), and
//! restricted venues are reported as `GEO_RESTRICTED` instead of being compared or routed.
//!
//! Policy data: `config/geo-policy.json` (compiled from each venue's official terms, with source
//! URLs), overridable with `OA_GEO_POLICY_FILE`. `OA_REQUIRE_USER_COUNTRY=true` (production) makes the
//! country mandatory for executable routes.

use oa_core::domain::PlatformId;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

const EMBEDDED: &str = include_str!("../../../config/geo-policy.json");

#[derive(Debug, Default)]
pub struct GeoPolicy {
    /// platform id -> restricted country codes.
    restricted: BTreeMap<String, BTreeSet<String>>,
    /// platform id -> countries where ONLY users are allowed (allow-list venues, e.g. Kalshi).
    allow_only: BTreeMap<String, BTreeSet<String>>,
    sources: BTreeMap<String, String>,
}

impl GeoPolicy {
    pub fn parse(json: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(json).map_err(|e| format!("geo policy: {e}"))?;
        let mut p = Self::default();
        let venues = v["venues"]
            .as_object()
            .ok_or("geo policy: missing venues")?;
        for (id, e) in venues {
            let set = |k: &str| -> BTreeSet<String> {
                e[k].as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.as_str())
                    .map(|c| c.trim().to_ascii_uppercase())
                    .filter(|c| valid_code(c))
                    .collect()
            };
            p.restricted.insert(id.clone(), set("restricted"));
            let allow = set("allowed_only");
            if !allow.is_empty() {
                p.allow_only.insert(id.clone(), allow);
            }
            if let Some(s) = e["source_url"].as_str() {
                p.sources.insert(id.clone(), s.to_string());
            }
        }
        Ok(p)
    }

    /// `Some(reason)` when users from `country` must not be sent to `platform`.
    pub fn restriction(&self, platform: PlatformId, country: &str) -> Option<String> {
        let id = platform.as_str();
        let src = self
            .sources
            .get(id)
            .map(|s| format!(" (source: {s})"))
            .unwrap_or_default();
        if let Some(allow) = self.allow_only.get(id)
            && !allow.contains(country)
        {
            return Some(format!("{id} only serves users from {allow:?}{src}"));
        }
        self.restricted
            .get(id)
            .filter(|r| r.contains(country))
            .map(|_| format!("{id} does not serve users from {country}{src}"))
    }
}

pub fn valid_code(c: &str) -> bool {
    c.len() == 2 && c.bytes().all(|b| b.is_ascii_uppercase())
}

/// Normalized ISO alpha-2 code, or an error message for the API.
pub fn normalize(country: &str) -> Result<String, String> {
    let c = country.trim().to_ascii_uppercase();
    if valid_code(&c) {
        Ok(c)
    } else {
        Err("user_country must be an ISO 3166-1 alpha-2 code (e.g. \"FR\", \"BR\")".into())
    }
}

pub fn policy() -> &'static GeoPolicy {
    static P: OnceLock<GeoPolicy> = OnceLock::new();
    P.get_or_init(|| {
        let custom = std::env::var("OA_GEO_POLICY_FILE")
            .ok()
            .and_then(|f| std::fs::read_to_string(f).ok());
        GeoPolicy::parse(custom.as_deref().unwrap_or(EMBEDDED)).unwrap_or_else(|e| {
            tracing::error!(error = %e, "invalid geo policy; falling back to the embedded one");
            GeoPolicy::parse(EMBEDDED).expect("embedded geo policy is valid (unit test)")
        })
    })
}

pub fn require_user_country() -> bool {
    std::env::var("OA_REQUIRE_USER_COUNTRY")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_policy_is_valid() {
        GeoPolicy::parse(EMBEDDED).unwrap();
    }

    #[test]
    fn restricted_and_allow_list_venues() {
        let p = GeoPolicy::parse(
            r#"{"venues": {"polymarket": {"restricted": ["US", "FR"], "source_url": "https://x"},
                            "kalshi": {"restricted": [], "allowed_only": ["US"]}}}"#,
        )
        .unwrap();
        assert!(p.restriction(PlatformId::Polymarket, "FR").is_some());
        assert!(p.restriction(PlatformId::Polymarket, "BR").is_none());
        assert!(p.restriction(PlatformId::Kalshi, "FR").is_some());
        assert!(p.restriction(PlatformId::Kalshi, "US").is_none());
        assert!(
            p.restriction(PlatformId::Deribit, "US").is_none(),
            "unknown venue = no data"
        );
        assert_eq!(normalize(" fr ").unwrap(), "FR");
        assert!(normalize("FRA").is_err());
    }
}
