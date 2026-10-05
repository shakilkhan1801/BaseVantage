//! `watchlist::*` — enrol, dedupe, collision refusal, cap, manual-only
//! removal. The bot never auto-removes.

mod common;

use alloy::primitives::Address;
use basevantage::watchlist::{EnrolOutcome, MemoryStore, Provenance, Watchlist};
use std::sync::Arc;

use common::TOKEN;

fn addr(n: u8) -> Address {
    Address::repeat_byte(n)
}

#[allow(dead_code)]
fn unused_store() -> Arc<MemoryStore> {
    Arc::new(MemoryStore::default())
}

#[test]
fn enrol_persists_with_provenance() {
    // Two lists sharing one store prove persistence across reloads.
    let store2 = Arc::new(MemoryStore::default());
    let list2 = Watchlist::new(50, Box::new(clone_store(&store2)));

    match list2.enrol(addr(0x01), "AAA", Provenance::Manual) {
        EnrolOutcome::Enrolled(e) => {
            assert_eq!(e.provenance, Provenance::Manual);
            assert_eq!(e.symbol, "AAA");
        }
        other => panic!("expected enrolment, got {:?}", std::mem::discriminant(&other)),
    }
    match list2.enrol(addr(0x02), "BBB", Provenance::Auto) {
        EnrolOutcome::Enrolled(e) => assert_eq!(e.provenance, Provenance::Auto),
        _ => panic!("expected enrolment"),
    }

    // Reload from the same store: entries and provenance survive.
    let reloaded = Watchlist::new(50, Box::new(clone_store(&store2)));
    let entries = reloaded.list();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].provenance, Provenance::Manual);
    assert_eq!(entries[1].provenance, Provenance::Auto);
}

#[test]
fn dedupe_by_address() {
    let list = Watchlist::new(50, Box::<MemoryStore>::default());
    assert!(matches!(
        list.enrol(TOKEN, "AAA", Provenance::Manual),
        EnrolOutcome::Enrolled(_)
    ));
    // Same address under a different symbol is still the same entry.
    let outcome = list.enrol(TOKEN, "ZZZ", Provenance::Auto);
    match outcome {
        EnrolOutcome::Refused(card) => {
            assert!(card.render().contains("already enrolled"));
            assert!(card.render().contains("already on the watchlist"));
        }
        EnrolOutcome::Enrolled(_) => panic!("duplicate address must be refused"),
    }
    assert_eq!(list.len(), 1);
}

#[test]
fn symbol_collision_refused() {
    let list = Watchlist::new(50, Box::<MemoryStore>::default());
    assert!(matches!(
        list.enrol(addr(0x01), "TWIN", Provenance::Manual),
        EnrolOutcome::Enrolled(_)
    ));
    // Same symbol, different address: collision refusal naming both addresses.
    match list.enrol(addr(0x02), "TWIN", Provenance::Auto) {
        EnrolOutcome::Refused(card) => {
            let rendered = card.render();
            assert!(rendered.contains("symbol collision"), "{rendered}");
            assert!(rendered.contains("0x01"), "must name the existing address: {rendered}");
            assert!(rendered.contains("0x02"), "must name the refused address: {rendered}");
        }
        EnrolOutcome::Enrolled(_) => panic!("symbol collision must be refused"),
    }
    assert_eq!(list.len(), 1);
}

#[test]
fn cap_50_refusal_card() {
    let list = Watchlist::new(50, Box::<MemoryStore>::default());
    for i in 0..50u8 {
        let symbol = format!("T{i:02}");
        assert!(matches!(
            list.enrol(addr(i + 1), &symbol, Provenance::Auto),
            EnrolOutcome::Enrolled(_)
        ));
    }
    assert_eq!(list.len(), 50);
    match list.enrol(addr(0xF0), "OVER", Provenance::Manual) {
        EnrolOutcome::Refused(card) => {
            let rendered = card.render();
            assert!(rendered.contains("watchlist full"), "{rendered}");
            assert!(rendered.contains("cap 50"), "{rendered}");
        }
        EnrolOutcome::Enrolled(_) => panic!("cap must refuse with a card"),
    }
    assert_eq!(list.len(), 50);
}

#[test]
fn manual_remove_only_bot_never_auto_removes() {
    let list = Watchlist::new(50, Box::<MemoryStore>::default());
    let mut ids = Vec::new();
    for (i, sym) in ["AAA", "BBB", "CCC"].iter().enumerate() {
        match list.enrol(addr(0x10 + i as u8), sym, Provenance::Manual) {
            EnrolOutcome::Enrolled(e) => ids.push(e.id),
            _ => panic!("expected enrolment"),
        }
    }

    // An automatic pass may annotate; it may never remove.
    let seen = list.sweep_annotate("refreshed by bot");
    assert_eq!(seen, 3);
    assert_eq!(list.len(), 3, "bot pass must not remove entries");
    assert!(list.list().iter().all(|e| e.note.as_deref() == Some("refreshed by bot")));

    // A second automatic pass still removes nothing.
    list.sweep_annotate("second pass");
    assert_eq!(list.len(), 3);

    // Removal exists only as the explicit manual API.
    let removed = list.remove(ids[1]).expect("manual remove works");
    assert_eq!(removed.symbol, "BBB");
    assert_eq!(list.len(), 2);
    assert!(list.remove(ids[1]).is_err());
}

/// MemoryStore isn't Clone; share one through a wrapper.
struct SharedStore(Arc<MemoryStore>);
impl basevantage::watchlist::WatchlistStore for SharedStore {
    fn load(&self) -> Vec<basevantage::watchlist::Entry> {
        self.0.load()
    }
    fn save(&self, entries: &[basevantage::watchlist::Entry]) -> basevantage::error::Result<()> {
        self.0.save(entries)
    }
}

fn clone_store(store: &Arc<MemoryStore>) -> SharedStore {
    SharedStore(store.clone())
}
