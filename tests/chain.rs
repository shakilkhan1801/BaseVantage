//! `chain::*` — RPC pool behaviour that needs no real chain: the 429
//! rate-limit backoff/retry in `RpcPool::with_provider` (a throttled endpoint
//! is alive, not dead, so it must be retried rather than scored down).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloy::providers::Provider;
use basevantage::chain::RpcPool;
use basevantage::error::EngineError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const RATE_LIMIT_BODY: &str =
    r#"{"jsonrpc":"2.0","id":ID,"error":{"code":-32016,"message":"over rate limit"}}"#;

/// Minimal JSON-RPC-over-HTTP server: the first request answers HTTP 429
/// (over rate limit), every later request answers `eth_chainId` = 1.
async fn spawn_server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let hits = hits2.clone();
            tokio::spawn(async move {
                loop {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let body_len = loop {
                        let n = match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(body_start) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
                            let declared: usize = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse().ok())
                                .unwrap_or(0);
                            if buf.len() >= body_start + 4 + declared {
                                break declared;
                            }
                        }
                    };
                    let body = &buf[buf.len() - body_len..];
                    let req: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                    let id = req.get("id").cloned().unwrap_or(serde_json::json!(1));
                    let n = hits.fetch_add(1, Ordering::SeqCst);
                    let payload = if n == 0 {
                        RATE_LIMIT_BODY.replace("ID", &id.to_string())
                    } else {
                        format!(r#"{{"jsonrpc":"2.0","id":{id},"result":"0x1"}}"#)
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{payload}",
                        payload.len(),
                        status = if n == 0 {
                            "429 Too Many Requests"
                        } else {
                            "200 OK"
                        },
                    );
                    if sock.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (format!("http://{addr}"), hits)
}

#[tokio::test]
async fn rate_limited_call_retries_with_backoff() {
    let (url, hits) = spawn_server().await;
    let pool = RpcPool::new(&[url]).unwrap();

    let chain_id = pool
        .with_provider(|p| async move {
            p.get_chain_id()
                .await
                .map_err(|e| EngineError::Rpc(e.to_string()))
        })
        .await
        .expect("retry after 429 must succeed");

    assert_eq!(chain_id, 1, "retried call returns the real answer");
    assert!(
        hits.load(Ordering::SeqCst) >= 2,
        "server must see both the throttled call and the retry"
    );
    assert_eq!(
        pool.health().healthy,
        1,
        "a rate-limited endpoint is throttled, not dead"
    );
}
