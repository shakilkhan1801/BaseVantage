use std::sync::{Arc, LazyLock};

use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::providers::WsConnect;
use alloy::rpc::types::{Filter, Log};
use futures::StreamExt;

use crate::chain::{EventFilter, EventStream, PoolEvent};
use crate::chain::base::RpcPool;

static SYNC_TOPIC: LazyLock<B256> = LazyLock::new(|| keccak256("Sync(uint112,uint112)"));
static V2_MINT_TOPIC: LazyLock<B256> = LazyLock::new(|| keccak256("Mint(address,uint256,uint256)"));
static V2_BURN_TOPIC: LazyLock<B256> = LazyLock::new(|| keccak256("Burn(address,uint256,uint256,address)"));

/// Decode one log into a pool event. Unknown logs from watched pools become
/// `SlotUpdate` so state is re-read; that keeps AMM-specific event shapes from
/// silently dropping state invalidations.
pub fn decode_event(address: Address, topics: &[B256], data: &Bytes, block: u64) -> Option<PoolEvent> {
    let topic0 = *topics.first()?;
    if topic0 == *SYNC_TOPIC {
        if data.len() < 64 {
            return None;
        }
        return Some(PoolEvent::Sync {
            pool: address,
            reserve0: U256::from_be_slice(&data[0..32]),
            reserve1: U256::from_be_slice(&data[32..64]),
            block,
        });
    }
    if topic0 == *V2_MINT_TOPIC {
        return Some(PoolEvent::Mint { pool: address, block });
    }
    if topic0 == *V2_BURN_TOPIC {
        return Some(PoolEvent::Burn { pool: address, block });
    }
    Some(PoolEvent::SlotUpdate { pool: address, block })
}

fn decode_rpc_log(log: &Log) -> Option<PoolEvent> {
    let topics = log.topics();
    let data = log.data().data.as_ref();
    decode_event(log.address(), topics, &Bytes::copy_from_slice(data), log.block_number.unwrap_or(0))
}

fn to_rpc_filter(filter: &EventFilter, from_block: u64, to_block: u64) -> Filter {
    Filter::new()
        .address(filter.addresses.clone())
        .from_block(from_block)
        .to_block(to_block)
}

/// WS-first stream of pool events. Falls back to HTTP `eth_getLogs` polling per
/// block when no WS endpoint answers, and keeps polling indefinitely.
pub fn stream_events(
    ws_urls: Vec<String>,
    filter: EventFilter,
    pool: Arc<RpcPool>,
) -> EventStream {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            if let Err(err) = ws_loop(&ws_urls, &filter, &tx).await {
                tracing::warn!("ws event source down ({err}); falling back to rpc polling");
            }
            if polling_loop(&filter, &pool, &tx).await.is_err() {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    });
    Box::pin(futures::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|event| (event, rx))
    }))
}

async fn ws_loop(
    ws_urls: &[String],
    filter: &EventFilter,
    tx: &tokio::sync::mpsc::UnboundedSender<PoolEvent>,
) -> Result<(), String> {
    let mut last_error = "no ws endpoints configured".to_string();
    for url in ws_urls {
        let ws = WsConnect::new(url);
        let provider = match ProviderBuilder::new().connect_ws(ws).await {
            Ok(p) => p,
            Err(e) => {
                last_error = format!("{url}: {e}");
                continue;
            }
        };
        let rpc_filter = Filter::new().address(filter.addresses.clone());
        let mut sub = match provider.subscribe_logs(&rpc_filter).await {
            Ok(s) => s.into_stream(),
            Err(e) => {
                last_error = format!("{url}: {e}");
                continue;
            }
        };
        tracing::info!("ws event source live: {url}");
        while let Some(log) = sub.next().await {
            if let Some(event) = decode_rpc_log(&log) {
                if tx.send(event).is_err() {
                    return Ok(());
                }
            }
        }
        last_error = format!("{url}: subscription ended");
    }
    Err(last_error)
}

async fn polling_loop(
    filter: &EventFilter,
    pool: &Arc<RpcPool>,
    tx: &tokio::sync::mpsc::UnboundedSender<PoolEvent>,
) -> Result<(), String> {
    let mut next_block = pool
        .with_provider(|p| async move {
            p.get_block_number().await.map_err(crate::chain::base::rpc_err)
        })
        .await
        .map_err(|e| e.to_string())?;
    loop {
        let head = pool
            .with_provider(|p| async move {
                p.get_block_number().await.map_err(crate::chain::base::rpc_err)
            })
            .await
        .map_err(|e| e.to_string())?;
        if head >= next_block {
            let rpc_filter = to_rpc_filter(filter, next_block, head);
            let logs = pool
                .with_provider(|p| {
                    let f = rpc_filter.clone();
                    async move { p.get_logs(&f).await.map_err(crate::chain::base::rpc_err) }
                })
                .await
        .map_err(|e| e.to_string())?;
            for log in &logs {
                if let Some(event) = decode_rpc_log(log) {
                    if tx.send(event).is_err() {
                        return Ok(());
                    }
                }
            }
            next_block = head + 1;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}
