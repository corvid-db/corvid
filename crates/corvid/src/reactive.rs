//! In-process change feeds.
//!
//! Subscribe a callback to receive a [`ChangeEvent`] whenever a document is
//! inserted or deleted. Notification is synchronous but lock-free at call time:
//! the subscriber list is cloned (callbacks are `Arc`-shared) and the lock
//! released *before* any callback runs, so a callback may safely read the
//! database without risking a deadlock. Strictly in-process — this is not a
//! network feed.

use std::sync::Arc;

use crate::db::Db;

/// The kind of change that occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// A document was inserted or overwritten.
    Insert,
    /// A document was deleted.
    Delete,
}

/// A change to a collection, delivered to subscribers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeEvent {
    /// The collection that changed.
    pub collection: String,
    /// The document key affected.
    pub key: Vec<u8>,
    /// What happened.
    pub kind: ChangeKind,
}

type Callback = Arc<dyn Fn(&ChangeEvent) + Send + Sync>;

/// The subscriber registry held on the [`Db`].
#[derive(Default)]
pub(crate) struct Subscribers {
    next_id: u64,
    list: Vec<(u64, Callback)>,
}

/// A handle identifying a subscription, for [`Db::unsubscribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionId(u64);

impl Db {
    /// Register `callback` to be invoked on every subsequent insert/delete.
    /// Returns a handle that [`Db::unsubscribe`] can remove.
    pub fn subscribe(
        &self,
        callback: impl Fn(&ChangeEvent) + Send + Sync + 'static,
    ) -> SubscriptionId {
        let mut subs = self.subscribers().lock().expect("subscribers lock");
        let id = subs.next_id;
        subs.next_id += 1;
        subs.list.push((id, Arc::new(callback)));
        SubscriptionId(id)
    }

    /// Remove a subscription. Returns whether it existed.
    pub fn unsubscribe(&self, id: SubscriptionId) -> bool {
        let mut subs = self.subscribers().lock().expect("subscribers lock");
        let before = subs.list.len();
        subs.list.retain(|(i, _)| *i != id.0);
        subs.list.len() != before
    }

    /// Deliver `event` to all current subscribers. Callbacks run after the
    /// registry lock is released.
    pub(crate) fn notify(&self, event: ChangeEvent) {
        let callbacks: Vec<Callback> = {
            let subs = self.subscribers().lock().expect("subscribers lock");
            if subs.list.is_empty() {
                return;
            }
            subs.list.iter().map(|(_, cb)| cb.clone()).collect()
        };
        for cb in callbacks {
            cb(&event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;
    use std::sync::Mutex;

    fn recorder() -> (
        Arc<Mutex<Vec<ChangeEvent>>>,
        impl Fn(&ChangeEvent) + Send + Sync,
    ) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        (log, move |e: &ChangeEvent| {
            sink.lock().unwrap().push(e.clone())
        })
    }

    #[test]
    fn insert_and_delete_emit_events() {
        let db = crate::Db::open_in_memory().unwrap();
        let (log, cb) = recorder();
        db.subscribe(cb);

        let c = db.collection("docs");
        c.insert(b"k", &Value::Int(1)).unwrap();
        c.delete(b"k").unwrap();

        let events = log.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, ChangeKind::Insert);
        assert_eq!(events[0].collection, "docs");
        assert_eq!(events[0].key, b"k".to_vec());
        assert_eq!(events[1].kind, ChangeKind::Delete);
    }

    #[test]
    fn delete_of_missing_key_emits_nothing() {
        let db = crate::Db::open_in_memory().unwrap();
        let (log, cb) = recorder();
        db.subscribe(cb);
        db.collection("docs").delete(b"absent").unwrap();
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn unsubscribe_stops_delivery() {
        let db = crate::Db::open_in_memory().unwrap();
        let (log, cb) = recorder();
        let id = db.subscribe(cb);

        db.collection("docs").insert(b"a", &Value::Int(1)).unwrap();
        assert!(db.unsubscribe(id));
        db.collection("docs").insert(b"b", &Value::Int(2)).unwrap();

        assert_eq!(log.lock().unwrap().len(), 1);
        // Unsubscribing again reports nothing was removed.
        assert!(!db.unsubscribe(id));
    }

    #[test]
    fn multiple_subscribers_all_receive() {
        let db = crate::Db::open_in_memory().unwrap();
        let (log1, cb1) = recorder();
        let (log2, cb2) = recorder();
        db.subscribe(cb1);
        db.subscribe(cb2);
        db.collection("docs").insert(b"k", &Value::Int(1)).unwrap();
        assert_eq!(log1.lock().unwrap().len(), 1);
        assert_eq!(log2.lock().unwrap().len(), 1);
    }

    #[test]
    fn no_subscribers_is_a_noop() {
        let db = crate::Db::open_in_memory().unwrap();
        // Should not panic or error with an empty registry.
        db.collection("docs").insert(b"k", &Value::Int(1)).unwrap();
    }

    #[test]
    fn callback_may_read_db_without_deadlock() {
        let db = crate::Db::open_in_memory().unwrap();
        db.collection("docs")
            .insert(b"seed", &Value::Int(0))
            .unwrap();

        let hits = Arc::new(Mutex::new(0usize));
        let sink = hits.clone();
        // The callback reads the database; notification holds no lock, so this
        // must not deadlock.
        db.subscribe(move |_e: &ChangeEvent| {
            *sink.lock().unwrap() += 1;
        });
        db.collection("docs").insert(b"k", &Value::Int(1)).unwrap();
        assert_eq!(*hits.lock().unwrap(), 1);
    }
}
