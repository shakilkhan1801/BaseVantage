use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::Address;

use crate::error::{EngineError, RefusalCard, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Provenance {
    Auto,
    Manual,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub id: u64,
    pub address: Address,
    pub symbol: String,
    pub provenance: Provenance,
    pub added_at_epoch_ms: u128,
    /// Sweeps and refreshes may annotate; they may never remove entries.
    pub note: Option<String>,
}

/// What an enrol attempt produced: an entry, or a refusal card.
#[derive(Debug)]
pub enum EnrolOutcome {
    Enrolled(Entry),
    Refused(RefusalCard),
}

/// Persistence boundary for watchlist entries.
pub trait WatchlistStore: Send + Sync {
    fn load(&self) -> Vec<Entry>;
    fn save(&self, entries: &[Entry]) -> Result<()>;
}

/// JSON-file store.
pub struct JsonFileStore {
    pub path: PathBuf,
}

impl WatchlistStore for JsonFileStore {
    fn load(&self) -> Vec<Entry> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, entries: &[Entry]) -> Result<()> {
        let json = serde_json::to_string_pretty(entries)
            .map_err(|e| EngineError::Watchlist(format!("serialize: {e}")))?;
        std::fs::write(&self.path, json)
            .map_err(|e| EngineError::Watchlist(format!("write {}: {e}", self.path.display())))
    }
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryStore {
    pub saved: Mutex<Vec<Entry>>,
}

impl WatchlistStore for MemoryStore {
    fn load(&self) -> Vec<Entry> {
        self.saved.lock().expect("poisoned").clone()
    }

    fn save(&self, entries: &[Entry]) -> Result<()> {
        *self.saved.lock().expect("poisoned") = entries.to_vec();
        Ok(())
    }
}

/// The watchlist: enrol with dedupe (address-then-symbol) and collision
/// refusal, hard cap with refusal card, manual-only removal. The bot never
/// auto-removes — automatic passes may only annotate.
pub struct Watchlist {
    entries: Mutex<Vec<Entry>>,
    cap: usize,
    store: Box<dyn WatchlistStore>,
    next_id: AtomicU64,
}

impl Watchlist {
    pub fn new(cap: usize, store: Box<dyn WatchlistStore>) -> Self {
        let entries = store.load();
        let next_id = entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        Self {
            entries: Mutex::new(entries),
            cap,
            store,
            next_id: AtomicU64::new(next_id),
        }
    }

    pub fn with_json_file(cap: usize, path: PathBuf) -> Self {
        Self::new(cap, Box::new(JsonFileStore { path }))
    }

    pub fn list(&self) -> Vec<Entry> {
        self.entries.lock().expect("poisoned").clone()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().expect("poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Enrol a token. Dedupe order: exact address match refused as duplicate,
    /// then symbol collision (same symbol, different address) refused with a
    /// card naming both addresses, then the cap check.
    pub fn enrol(&self, address: Address, symbol: &str, provenance: Provenance) -> EnrolOutcome {
        let mut entries = self.entries.lock().expect("poisoned");

        if entries.iter().any(|e| e.address == address) {
            return EnrolOutcome::Refused(
                RefusalCard::new("already enrolled")
                    .line(format!("address {address} is already on the watchlist")),
            );
        }
        if let Some(existing) = entries.iter().find(|e| e.symbol == symbol) {
            return EnrolOutcome::Refused(
                RefusalCard::new("symbol collision")
                    .line(format!(
                        "symbol {symbol} already enrolled at {}",
                        existing.address
                    ))
                    .line(format!("refusing to enrol {address} under the same symbol")),
            );
        }
        if entries.len() >= self.cap {
            return EnrolOutcome::Refused(
                RefusalCard::new("watchlist full")
                    .line(format!("cap {} reached", self.cap))
                    .line(format!("refusing to enrol {address} ({symbol})")),
            );
        }

        let entry = Entry {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            address,
            symbol: symbol.to_string(),
            provenance,
            added_at_epoch_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            note: None,
        };
        entries.push(entry.clone());
        let _ = self.store.save(&entries);
        EnrolOutcome::Enrolled(entry)
    }

    /// Manual-only removal. There is no automatic removal path: `remove` is
    /// the single deletion API and it is only invoked by an explicit operator
    /// action.
    pub fn remove(&self, id: u64) -> Result<Entry> {
        let mut entries = self.entries.lock().expect("poisoned");
        let pos = entries
            .iter()
            .position(|e| e.id == id)
            .ok_or_else(|| EngineError::Watchlist(format!("no entry {id}")))?;
        let removed = entries.remove(pos);
        self.store.save(&entries)?;
        Ok(removed)
    }

    /// Annotate one entry (what automatic passes are allowed to do).
    pub fn annotate(&self, id: u64, note: &str) -> Result<()> {
        let mut entries = self.entries.lock().expect("poisoned");
        let entry = entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| EngineError::Watchlist(format!("no entry {id}")))?;
        entry.note = Some(note.to_string());
        self.store.save(&entries)
    }

    /// An automatic sweep pass: may annotate every entry, may never remove.
    /// Returns the number of entries it saw.
    pub fn sweep_annotate(&self, note: &str) -> usize {
        let mut entries = self.entries.lock().expect("poisoned");
        for entry in entries.iter_mut() {
            entry.note = Some(note.to_string());
        }
        let count = entries.len();
        let _ = self.store.save(&entries);
        count
    }
}
