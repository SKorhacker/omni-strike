//! Network relabelling: present a connector under another network label (e.g. the same parser
//! pointed at a venue's TESTNET host). Instrument keys, book keys and bulk tops are all retagged so
//! they match the testnet execution venue's `network()`; the inner connector still sees its own label.

use crate::{Connector, Implemented, MarketData, Result};
use async_trait::async_trait;
use oa_core::domain::*;
use oa_core::fees::FeeSchedule;
use std::time::Duration;

pub struct OnNetwork<C> {
    inner: C,
    network: &'static str,
}

impl<C: Connector> OnNetwork<C> {
    pub fn new(inner: C, network: &'static str) -> Self {
        Self { inner, network }
    }

    fn inner_view(&self, i: &Instrument) -> Instrument {
        let mut x = i.clone();
        x.key.network = self.inner.network().into();
        x
    }
}

#[async_trait]
impl<C: Connector> Connector for OnNetwork<C> {
    fn platform(&self) -> PlatformId {
        self.inner.platform()
    }
    fn network(&self) -> &'static str {
        self.network
    }
    fn implemented(&self, cap: Capability) -> Implemented {
        self.inner.implemented(cap)
    }
    fn streams_books(&self) -> bool {
        self.inner.streams_books()
    }
    fn prewarm(&self, instruments: &[Instrument]) {
        let v: Vec<Instrument> = instruments.iter().map(|i| self.inner_view(i)).collect();
        self.inner.prewarm(&v)
    }
    fn hot_refresh_interval(&self) -> Duration {
        self.inner.hot_refresh_interval()
    }
    fn bulk_refresh_interval(&self) -> Duration {
        self.inner.bulk_refresh_interval()
    }
    fn diagnostics(&self) -> Option<serde_json::Value> {
        self.inner.diagnostics()
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let mut v = self.inner.list_instruments(underlying).await?;
        for i in &mut v {
            i.key.network = self.network.into();
        }
        Ok(v)
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<MarketData> {
        let mut md = self
            .inner
            .get_market_data(&self.inner_view(instrument))
            .await?;
        md.book.instrument.network = self.network.into();
        Ok(md)
    }

    async fn bulk_top_of_book(&self, underlying: &str) -> Result<Option<Vec<BookSnapshot>>> {
        Ok(self.inner.bulk_top_of_book(underlying).await?.map(|v| {
            v.into_iter()
                .map(|mut b| {
                    b.instrument.network = self.network.into();
                    b
                })
                .collect()
        }))
    }

    fn get_fees(&self, instrument: &Instrument) -> Result<FeeSchedule> {
        self.inner.get_fees(&self.inner_view(instrument))
    }
}
