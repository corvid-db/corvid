//! Feature-gated instrumentation shim (Task 11).
//!
//! The engine's zero-dependency default build and its WASM size budget are
//! contracts; observability is not allowed to break either. Every
//! instrumentation call site in the engine goes through this module, so the
//! feature boundary lives in exactly one place:
//!
//! * **`--features tracing`**: [`span!`] and [`event!`] pass through to the
//!   `tracing` crate (one added dependency), emitting spans/events at the
//!   load-bearing points — index backfill pages, compaction trigger math,
//!   lazy resumes and adjacency rebuilds, query plan-shape selection,
//!   edge-cascade fallbacks. Granularity is per-operation / per-page,
//!   never per-document.
//! * **default (feature off)**: both macros expand away — the captured
//!   tokens (field names AND their value expressions) are dropped before
//!   name resolution, so nothing is evaluated, no code is generated, and
//!   the `tracing` dependency is not in the graph at all. [`span!`]
//!   expands to the zero-sized [`NoSpan`] guard so call sites keep the
//!   `let _guard = ...` shape under both configurations.
//!
//! Spans returned by [`span!`] are already entered (a RAII guard binds to a
//! `let _guard = ...`); events are fire-and-forget. Field values that are
//! not `tracing::Value` types natively (borrowed strings, byte cursors) use
//! [`display`] / [`debug`], re-exported here for the same both-ways compile.
//!
//! This module is engine-private (`pub(crate)`) — enabling the feature adds
//! no public API surface.

#[cfg(feature = "tracing")]
pub(crate) use tracing::field::{debug, display};

/// The zero-sized span guard the off-mode [`span!`] evaluates to — keeps the
/// `let _guard = span!(...)` call-site shape meaningful (a non-unit binding)
/// while costing nothing.
#[cfg(not(feature = "tracing"))]
#[derive(Debug, Default)]
pub(crate) struct NoSpan;

/// An entered span (RAII guard) on the feature; a no-op unit value off it.
///
/// Call shape (identical source under both configurations):
///
/// ```text
/// let _guard = telemetry::span!(DEBUG, "index_backfill_page",
///     collection = telemetry::display(collection),
///     kind = telemetry::display(kind),
///     docs = page.len() as u64);
/// ```
#[cfg(feature = "tracing")]
macro_rules! span {
    ($lvl:ident, $name:literal $(, $field:ident = $value:expr)* $(,)?) => {
        tracing::span!(
            target: "corvid",
            tracing::Level::$lvl,
            $name
            $(, $field = $value)*
        )
        .entered()
    };
}

#[cfg(feature = "tracing")]
macro_rules! event {
    ($lvl:ident, $($fields:tt)+) => {
        tracing::event!(target: "corvid", tracing::Level::$lvl, $($fields)+)
    };
}

#[cfg(not(feature = "tracing"))]
macro_rules! span {
    ($lvl:ident, $name:literal $(, $field:ident = $value:expr)* $(,)?) => {
        crate::telemetry::NoSpan
    };
}

#[cfg(not(feature = "tracing"))]
macro_rules! event {
    ($lvl:ident, $($fields:tt)+) => {{}};
}

#[cfg(feature = "tracing")]
pub(crate) use event;
#[cfg(not(feature = "tracing"))]
pub(crate) use event;
#[cfg(feature = "tracing")]
pub(crate) use span;
#[cfg(not(feature = "tracing"))]
pub(crate) use span;

/// Conformance for the feature-gated instrumentation: with
/// `--features tracing`, one index build and two query shapes must actually
/// emit their spans/events. The subscriber is hand-rolled (no
/// `tracing-subscriber` dev-dependency — the feature's whole point is
/// minimal footprint): it records every call-site this shim owns (asserted
/// via the `corvid` target) into a shared log that the assertions grep.
///
/// Compiles only under the feature; the default build's zero-dep posture is
/// enforced elsewhere (CI's `cargo tree` assertion + the wasm job).
#[cfg(all(test, feature = "tracing"))]
mod tests {
    use std::sync::{Arc, Mutex, OnceLock};

    use tracing::field::{Field, Visit};
    use tracing::span::Attributes;
    use tracing::{Event, Id, Subscriber};

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    struct Grep<'a>(&'a mut String);

    impl Visit for Grep<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write;
            let _ = write!(self.0, " {}={:?}", field.name(), value);
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            use std::fmt::Write;
            let _ = write!(self.0, " {}={}", field.name(), value);
        }
        fn record_f64(&mut self, field: &Field, value: f64) {
            use std::fmt::Write;
            let _ = write!(self.0, " {}={}", field.name(), value);
        }
    }

    impl Subscriber for Recorder {
        fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
            // The shim's pass-through sets `target: "corvid"`; accepting only
            // that target both filters other crates' noise and pins the
            // target contract.
            meta.target() == "corvid"
        }
        fn new_span(&self, span: &Attributes<'_>) -> Id {
            self.0
                .lock()
                .unwrap()
                .push(format!("span {}", span.metadata().name()));
            Id::from_u64(1)
        }
        fn event(&self, event: &Event<'_>) {
            let mut line = format!("event {}", event.metadata().name());
            event.record(&mut Grep(&mut line));
            self.0.lock().unwrap().push(line);
        }
        fn record(&self, _span: &Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
        fn enter(&self, _span: &Id) {}
        fn exit(&self, _span: &Id) {}
    }

    fn install() -> Recorder {
        static REC: OnceLock<Recorder> = OnceLock::new();
        let rec = REC.get_or_init(Recorder::default).clone();
        let _ = tracing::subscriber::set_global_default(rec.clone());
        rec
    }

    fn doc(n: i64) -> crate::Value {
        crate::Value::Map(
            [("n".to_owned(), crate::Value::Int(n))]
                .into_iter()
                .collect(),
        )
    }

    fn vector_doc(v: Vec<f32>) -> crate::Value {
        crate::Value::Map(
            [("embedding".to_owned(), crate::Value::Vector(v))]
                .into_iter()
                .collect(),
        )
    }

    #[test]
    fn tracing_feature_instrumentation_fires_for_build_and_query() {
        let rec = install();
        let db = crate::Db::open_in_memory().unwrap();
        let c = db.collection("tr_docs");
        for i in 0..10i64 {
            c.insert(format!("k{i}").as_bytes(), &doc(i)).unwrap();
        }

        // Build: the atomic backfill driver must span its page(s) and emit
        // the completion event carrying the index family.
        rec.0.lock().unwrap().clear();
        c.create_scalar_index("n").unwrap();
        let log = rec.0.lock().unwrap().join("\n");
        assert!(
            log.contains("span index_backfill_page"),
            "backfill page span missing:\n{log}"
        );
        assert!(
            log.contains("message=\"index_backfill_complete\""),
            "completion event missing:\n{log}"
        );
        assert!(
            log.contains("kind=scalar"),
            "index-kind label missing:\n{log}"
        );
        assert!(
            log.contains("collection=tr_docs"),
            "collection field missing:\n{log}"
        );

        // Query, indexed arm: the plan-shape event names the window family.
        rec.0.lock().unwrap().clear();
        let rows = c
            .query()
            .filter(crate::field("n").ge(crate::Value::Int(2)))
            .limit(3)
            .run()
            .unwrap();
        assert_eq!(rows.len(), 3);
        let log = rec.0.lock().unwrap().join("\n");
        assert!(
            log.contains("message=\"plan_shape\""),
            "plan-shape event missing:\n{log}"
        );
        assert!(
            log.contains("shape=\"indexed_window\""),
            "indexed_window shape missing:\n{log}"
        );

        // Query, order-index walk arm: the walk-vs-pointgets decision is
        // visible (a filterless order_by over the indexed field).
        rec.0.lock().unwrap().clear();
        let rows = c.query().order_by("n", true).limit(4).run().unwrap();
        assert_eq!(rows.len(), 4);
        let log = rec.0.lock().unwrap().join("\n");
        assert!(
            log.contains("shape=\"sort_index\""),
            "sort_index shape missing:\n{log}"
        );
    }

    /// Task 12 prepend (a): `inmemory_compaction`'s `live` field must count
    /// nodes still serving searches (total − tombstoned), matching the
    /// on-disk `ondisk_compaction` event's semantics — NOT
    /// `node_to_key.len()`, which counts tombstoned slots too. Corpus: one
    /// document whose vector is overwritten 20 times — each overwrite
    /// tombstones the old node and adds a new one, so at the trigger the
    /// graph holds 21 total slots, 20 dead, 1 live (dead-majority → the
    /// next search compacts). The buggy field reported live=21.
    #[test]
    fn inmemory_compaction_event_reports_live_nodes() {
        let rec = install();
        let db = crate::Db::open_in_memory().unwrap();
        let c = db.collection("tr_compact");
        c.create_vector_index("embedding", crate::Metric::L2)
            .unwrap();
        c.insert(b"k", &vector_doc(vec![0.0, 0.0])).unwrap();
        // First search lazily builds the graph (1 node, 0 dead).
        let _ = c
            .vector_search("embedding", &[0.0, 0.0], 1, crate::Metric::L2)
            .unwrap();
        for i in 0..20 {
            c.insert(b"k", &vector_doc(vec![i as f32, 0.0])).unwrap();
        }
        rec.0.lock().unwrap().clear();
        let _ = c
            .vector_search("embedding", &[0.0, 0.0], 1, crate::Metric::L2)
            .unwrap();
        let log = rec.0.lock().unwrap().join("\n");
        assert!(
            log.contains("message=\"inmemory_compaction\""),
            "compaction event missing (trigger should have crossed):\n{log}"
        );
        assert!(
            log.contains("dead=20 live=1"),
            "live must be live nodes (total − dead), not node_to_key.len():\n{log}"
        );
    }
}
