use std::sync::Arc;

use crate::chain::{DynChain, EventFilter, PoolEvent};
use crate::market::MarketData;

/// Subscribe to pool events and keep the market cache current: `Sync` events
/// patch reserves in place (WS-driven freshness), everything else invalidates
/// so the next read refetches.
pub fn spawn_event_updates(
    market: Arc<MarketData>,
    chain: DynChain,
    filter: EventFilter,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut events = chain.events(filter);
        use futures::StreamExt;
        while let Some(event) = events.next().await {
            market.apply_event(&event);
        }
    })
}

/// Convenience for tests: apply a batch of events at once.
pub fn apply_events(market: &MarketData, events: &[PoolEvent]) {
    for event in events {
        market.apply_event(event);
    }
}
