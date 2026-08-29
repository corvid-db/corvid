//! Logical dump / load for migration.
//!
//! The on-disk storage format may break before v1.0 (per project policy: no
//! backward-compat shims). [`Db::dump`] writes a *logical*, version-stamped
//! export — every user collection's documents, all index/schema/TTL
//! definitions, the graph edges, and the auto-key counters — and [`Db::load`]
//! replays it into a fresh database. A format break is migrated by `dump`
//! from the old binary and `load` into the new one.
//!
//! The dump is independent of the redb layout and the reserved-collection
//! encodings (which are what change); document *values* use the [`Value`]
//! codec, and indexes are *recreated* (rebuilt from the loaded documents)
//! rather than copying their derived internal state.
//!
//! [`Db::load_with_renames`] is the collection-rename path for dumps from
//! pre-wave-4 databases whose collection names contain `__` (rejected by
//! name validation since): every collection-name occurrence in the stream
//! is rewritten through a caller-supplied map before replay.

use std::collections::BTreeMap;
use std::io::{Read, Write};

use crate::db::Db;
use crate::error::{Error, Result};
use crate::index::VectorMode;
use crate::schema::{Field, FieldType, Schema};
use crate::value::Value;
use crate::{Metric, Quantization};

const MAGIC: &[u8] = b"CORVIDDUMPv1";

// ---- byte writers ----

fn put_u32(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u32).to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u64).to_le_bytes());
}
fn put_i64(out: &mut Vec<u8>, n: i64) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn put_f64(out: &mut Vec<u8>, n: f64) {
    out.extend_from_slice(&n.to_bits().to_le_bytes());
}
fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u32(out, b.len());
    out.extend_from_slice(b);
}
fn put_str(out: &mut Vec<u8>, s: &str) {
    put_bytes(out, s.as_bytes());
}

// ---- byte reader ----

/// Streaming primitive reader over any [`Read`] (audit B8): the dump is
/// consumed through bounded, exact reads (`BufReader` underneath) instead of
/// `read_to_end`, so loading never holds the whole file in memory. Length
/// prefixes are honored allocation-conservatively: variable-size fields grow
/// in chunks, so a forged length cannot drive a huge up-front allocation.
struct Reader<R> {
    r: R,
}

impl<R: Read> Reader<R> {
    fn new(r: R) -> Self {
        Reader { r }
    }

    /// Read exactly `buf.len()` bytes; a short stream is an `InvalidDump`
    /// (not a raw I/O error), matching the slice-based reader this replaces.
    fn exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.r.read_exact(buf).map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => {
                Error::InvalidDump("unexpected end of dump".into())
            }
            _ => Error::Io(e),
        })
    }
    fn u8(&mut self) -> Result<u8> {
        let mut b = [0u8; 1];
        self.exact(&mut b)?;
        Ok(b[0])
    }
    fn u32(&mut self) -> Result<usize> {
        let mut b = [0u8; 4];
        self.exact(&mut b)?;
        Ok(u32::from_le_bytes(b) as usize)
    }
    fn u64(&mut self) -> Result<usize> {
        let mut b = [0u8; 8];
        self.exact(&mut b)?;
        Ok(u64::from_le_bytes(b) as usize)
    }
    fn i64(&mut self) -> Result<i64> {
        let mut b = [0u8; 8];
        self.exact(&mut b)?;
        Ok(i64::from_le_bytes(b))
    }
    fn f64(&mut self) -> Result<f64> {
        let mut b = [0u8; 8];
        self.exact(&mut b)?;
        Ok(f64::from_bits(u64::from_le_bytes(b)))
    }
    fn u64_raw(&mut self) -> Result<u64> {
        let mut b = [0u8; 8];
        self.exact(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }
    /// Read a length-prefixed byte field. The u32 length is untrusted: the
    /// buffer grows in bounded chunks as bytes actually arrive, so a forged
    /// huge length fails on truncated input without first allocating it.
    fn bytes(&mut self) -> Result<Vec<u8>> {
        const CHUNK: usize = 64 * 1024;
        let n = self.u32()?;
        let mut out = Vec::new();
        let mut chunk = vec![0u8; n.min(CHUNK)];
        let mut remaining = n;
        while remaining > 0 {
            let want = remaining.min(chunk.len());
            self.exact(&mut chunk[..want])?;
            out.extend_from_slice(&chunk[..want]);
            remaining -= want;
        }
        Ok(out)
    }
    fn string(&mut self) -> Result<String> {
        let b = self.bytes()?;
        String::from_utf8(b).map_err(|_| Error::InvalidDump("non-utf8 string".into()))
    }
}

fn metric_byte(m: Metric) -> u8 {
    match m {
        Metric::Cosine => 0,
        Metric::Dot => 1,
        Metric::L2 => 2,
    }
}
fn metric_from(b: u8) -> Result<Metric> {
    Ok(match b {
        0 => Metric::Cosine,
        1 => Metric::Dot,
        2 => Metric::L2,
        _ => return Err(Error::InvalidDump("bad metric".into())),
    })
}
fn quant_byte(q: Quantization) -> u8 {
    match q {
        Quantization::None => 0,
        Quantization::Binary => 1,
        Quantization::Scalar => 2,
    }
}
fn quant_from(b: u8) -> Result<Quantization> {
    Ok(match b {
        0 => Quantization::None,
        1 => Quantization::Binary,
        2 => Quantization::Scalar,
        _ => return Err(Error::InvalidDump("bad quant".into())),
    })
}

impl Db {
    /// Write a logical, version-stamped dump of the whole database to `w`:
    /// every user document plus all index, schema, and TTL definitions. Load it
    /// into a fresh database with [`Db::load`].
    ///
    /// One read snapshot covers the WHOLE dump (audit B8): the catalog walk,
    /// every collection's records, the TTL and edge namespaces, and the
    /// auto-id counters all observe a single point in time, so a dump taken
    /// while writers are active is never torn. Records stream through
    /// `for_each` in two passes (count, then emit — the format prefixes a
    /// record count and `w` may be unseekable); memory stays bounded by the
    /// largest single record, not the corpus. Index/schema definitions come
    /// from the in-memory registries, which mirror committed state; TTL and
    /// edge entries are enumerated through the same snapshot. The snapshot
    /// stays open for the duration of the write to `w`: writers are never
    /// blocked (MVCC), but a very slow sink extends the read transaction's
    /// lifetime, like any long query.
    pub fn dump<W: Write>(&self, w: W) -> Result<()> {
        let mut w = std::io::BufWriter::new(w);
        w.write_all(MAGIC)?;

        self.store().read(|r| {
            let collections: Vec<String> = r
                .collections()?
                .into_iter()
                .filter(|n| !n.starts_with("__"))
                .collect();

            // Records: (collection, key, encoded value). Pass 1 counts.
            let mut n_records: u64 = 0;
            for coll in &collections {
                r.for_each(coll, &mut |_, _| {
                    n_records += 1;
                    Ok(true)
                })?;
            }
            w.write_all(&n_records.to_le_bytes())?;
            // Pass 2 emits each record as it streams past.
            let mut rec = Vec::new();
            for coll in &collections {
                r.for_each(coll, &mut |key, value| {
                    rec.clear();
                    put_str(&mut rec, coll);
                    put_bytes(&mut rec, key);
                    put_bytes(&mut rec, value);
                    w.write_all(&rec)?;
                    Ok(true)
                })?;
            }

            // Vector indexes (from the in-memory registry).
            let mut buf = Vec::new();
            let vectors = self.vector_specs();
            put_u64(&mut buf, vectors.len());
            for v in &vectors {
                put_str(&mut buf, &v.collection);
                put_str(&mut buf, &v.field);
                buf.push(metric_byte(v.metric));
                buf.push(quant_byte(v.quant));
                match v.mode {
                    VectorMode::InMemory => buf.push(0),
                    VectorMode::OnDisk => buf.push(1),
                    VectorMode::OnDiskPq { m, k } => {
                        buf.push(2);
                        put_u32(&mut buf, m);
                        put_u32(&mut buf, k);
                    }
                }
            }

            // Text indexes.
            let texts = self.text_specs();
            put_u64(&mut buf, texts.len());
            for (coll, field, on_disk) in &texts {
                put_str(&mut buf, coll);
                put_str(&mut buf, field);
                buf.push(*on_disk as u8);
            }

            // Scalar indexes.
            let scalars = self.scalar_specs();
            put_u64(&mut buf, scalars.len());
            for (coll, field) in &scalars {
                put_str(&mut buf, coll);
                put_str(&mut buf, field);
            }

            // Compound indexes.
            let compounds = self.compound_specs();
            put_u64(&mut buf, compounds.len());
            for (coll, fields) in &compounds {
                put_str(&mut buf, coll);
                put_u32(&mut buf, fields.len());
                for f in fields {
                    put_str(&mut buf, f);
                }
            }

            // Geo indexes.
            let geos = self.geo_specs();
            put_u64(&mut buf, geos.len());
            for (coll, field) in &geos {
                put_str(&mut buf, coll);
                put_str(&mut buf, field);
            }

            // Schemas.
            let schemas = self.schema_specs();
            put_u64(&mut buf, schemas.len());
            for (coll, schema) in &schemas {
                put_str(&mut buf, coll);
                put_u32(&mut buf, schema.fields().len());
                for f in schema.fields() {
                    buf.push(f.ty.to_byte());
                    buf.push(f.required as u8);
                    buf.push(f.unique as u8);
                    put_str(&mut buf, &f.name);
                }
            }
            w.write_all(&buf)?;

            // TTLs (same snapshot: through `r`, enumerating the persisted
            // `__ttl__*` namespaces from the catalog on that snapshot).
            let ttls = crate::ttl::ttl_specs_in(r)?;
            let mut buf = Vec::new();
            put_u64(&mut buf, ttls.len());
            for (coll, key, expiry) in &ttls {
                put_str(&mut buf, coll);
                put_bytes(&mut buf, key);
                put_i64(&mut buf, *expiry);
            }
            w.write_all(&buf)?;

            // Auto-id counters, so `insert_auto` never re-issues a used key
            // after a dump→load cycle (same snapshot as the records: a
            // counter ahead of the documents it named would tear the dump).
            let autos = r.auto_ids()?;
            let mut buf = Vec::new();
            put_u64(&mut buf, autos.len());
            for (coll, next) in &autos {
                put_str(&mut buf, coll);
                buf.extend_from_slice(&next.to_le_bytes());
            }
            w.write_all(&buf)?;

            // Graph edges (forward+reverse are rebuilt by `load` via `link`;
            // same snapshot: through `r`).
            let edges = self.all_edges_in(r)?;
            let mut buf = Vec::new();
            put_u64(&mut buf, edges.len());
            for e in &edges {
                put_str(&mut buf, &e.collection);
                put_str(&mut buf, &e.relation);
                put_bytes(&mut buf, &e.from);
                put_bytes(&mut buf, &e.to);
                put_f64(&mut buf, e.weight);
            }
            w.write_all(&buf)?;
            Ok(())
        })?;
        w.flush()?;
        Ok(())
    }

    /// Replay a [`Db::dump`] into this (fresh) database: documents are written,
    /// then indexes are recreated (rebuilt from the documents), schemas
    /// declared, and TTLs restored. The dump streams through a buffered
    /// reader — memory is bounded by the largest single field, never the
    /// whole file (audit B8). Equivalent to
    /// [`load_with_renames`](Db::load_with_renames) with an empty map.
    pub fn load<R: Read>(&self, r: R) -> Result<()> {
        self.load_with_renames(r, &BTreeMap::new())
    }

    /// [`Db::load`] with a collection-RENAME map — the migration path for
    /// dumps from pre-wave-4 databases whose collection names contain `__`
    /// (interior double underscore; rejected by name validation since, so
    /// [`Db::load`] fails such a dump at index/schema replay with
    /// [`Error::InvalidName`]). Every collection-name occurrence in the dump
    /// stream — records, all index definitions, schemas, TTL entries, graph
    /// edges, auto-id counters — is mapped through `renames` before replay,
    /// so documents and their definitions land together under the target
    /// name; index definitions replay via the create-* backfill path, which
    /// reads the records already written under the target name, so every
    /// index is rebuilt under the new name automatically (nothing to
    /// re-create by hand).
    ///
    /// Contract:
    ///
    /// - Each target must be a valid user collection name (no `__` sequence,
    ///   no NUL byte): an invalid target fails the load with that target's
    ///   [`Error::InvalidName`] before the stream is read.
    /// - No two dump names may load into one output name — neither two map
    ///   sources sharing a target, nor a target colliding with another
    ///   (mapped or unmapped) dump collection. Either would merge two
    ///   collections' rows into one keyspace, silently overwriting
    ///   documents; both fail with [`Error::InvalidArgument`]. (Loading into
    ///   a non-empty database still MERGES with pre-existing collections,
    ///   exactly like [`Db::load`].)
    /// - Engine-reserved (`__`-prefixed) dump names are rejected exactly as
    ///   in [`Db::load`], before mapping: a rename cannot launder an
    ///   engine-internal namespace into a user name.
    /// - A map entry whose source never occurs in the dump is a no-op; names
    ///   not in the map pass through unchanged.
    ///
    /// The recipe: dump the old database with the old binary version (or use
    /// an existing dump file), open a fresh database with the current
    /// engine, and `load_with_renames` with `{ "a__b": "a_b", … }`.
    pub fn load_with_renames<R: Read>(
        &self,
        r: R,
        renames: &BTreeMap<String, String>,
    ) -> Result<()> {
        let mut renames = Renames::new(renames)?;
        let mut rd = Reader::new(std::io::BufReader::new(r));

        let mut magic = [0u8; MAGIC.len()];
        rd.exact(&mut magic)?;
        if magic.as_slice() != MAGIC {
            return Err(Error::InvalidDump(
                "bad magic / unknown dump version".into(),
            ));
        }

        // Records → store directly (indexes are recreated afterwards). The
        // `__`-prefixed namespaces are engine-internal; a dump that names one
        // is malformed (or hostile), since no legitimate dump can contain it.
        let n_records = rd.u64()?;
        for _ in 0..n_records {
            let coll = renames.apply("records", rd.string()?)?;
            let key = rd.bytes()?;
            let value = rd.bytes()?;
            // Validate the value decodes under the current codec.
            Value::decode(&value)?;
            self.store().put(&coll, &key, &value)?;
        }

        // Vector indexes.
        let n_vec = rd.u64()?;
        for _ in 0..n_vec {
            let coll = renames.apply("vector index", rd.string()?)?;
            let field = rd.string()?;
            let metric = metric_from(rd.u8()?)?;
            let quant = quant_from(rd.u8()?)?;
            let c = self.collection(&coll);
            match rd.u8()? {
                0 => c.create_vector_index_quantized(&field, metric, quant)?,
                1 => c.create_vector_index_ondisk_quantized(&field, metric, quant)?,
                2 => {
                    let m = rd.u32()?;
                    let k = rd.u32()?;
                    c.create_vector_index_ondisk_pq(&field, metric, m, k)?;
                }
                _ => return Err(Error::InvalidDump("bad vector mode".into())),
            }
        }

        // Text indexes.
        let n_text = rd.u64()?;
        for _ in 0..n_text {
            let coll = renames.apply("text index", rd.string()?)?;
            let field = rd.string()?;
            let on_disk = rd.u8()? != 0;
            let c = self.collection(&coll);
            if on_disk {
                c.create_text_index_ondisk(&field)?;
            } else {
                c.create_text_index(&field)?;
            }
        }

        // Scalar indexes.
        let n_scalar = rd.u64()?;
        for _ in 0..n_scalar {
            let coll = renames.apply("scalar index", rd.string()?)?;
            let field = rd.string()?;
            self.collection(&coll).create_scalar_index(&field)?;
        }

        // Compound indexes.
        let n_compound = rd.u64()?;
        for _ in 0..n_compound {
            let coll = renames.apply("index", rd.string()?)?;
            let nf = rd.u32()?;
            // The count is untrusted input; allocate conservatively and grow.
            let mut fields = Vec::with_capacity(nf.min(4096));
            for _ in 0..nf {
                fields.push(rd.string()?);
            }
            let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
            self.collection(&coll).create_compound_index(&refs)?;
        }

        // Geo indexes.
        let n_geo = rd.u64()?;
        for _ in 0..n_geo {
            let coll = renames.apply("geo index", rd.string()?)?;
            let field = rd.string()?;
            self.collection(&coll).create_geo_index(&field)?;
        }

        // Schemas.
        let n_schema = rd.u64()?;
        for _ in 0..n_schema {
            let coll = renames.apply("schema", rd.string()?)?;
            let nf = rd.u32()?;
            let mut schema = Schema::new();
            for _ in 0..nf {
                let ty = FieldType::from_byte(rd.u8()?)
                    .ok_or_else(|| Error::InvalidDump("bad field type".into()))?;
                let required = rd.u8()? != 0;
                let unique = rd.u8()? != 0;
                let name = rd.string()?;
                let mut f = Field::new(name, ty);
                if required {
                    f = f.required();
                }
                if unique {
                    f = f.unique();
                }
                schema = schema.field(f);
            }
            self.collection(&coll).set_schema(&schema)?;
        }

        // TTLs.
        let n_ttl = rd.u64()?;
        for _ in 0..n_ttl {
            let coll = renames.apply("TTL", rd.string()?)?;
            let key = rd.bytes()?;
            let expiry = rd.i64()?;
            self.collection(&coll).set_ttl(&key, expiry)?;
        }

        // Auto-id counters (counters never move backwards on restore).
        let n_auto = rd.u64()?;
        let mut autos = Vec::with_capacity(n_auto.min(4096));
        for _ in 0..n_auto {
            let coll = renames.apply("auto-ids", rd.string()?)?;
            let next = rd.u64_raw()?;
            autos.push((coll, next));
        }
        self.store().restore_auto_ids(&autos)?;

        // Graph edges: replayed through `link_weighted` so each edge's
        // forward+reverse pair is rebuilt atomically.
        let n_edges = rd.u64()?;
        for _ in 0..n_edges {
            let coll = renames.apply("edges", rd.string()?)?;
            let rel = rd.string()?;
            let from = rd.bytes()?;
            let to = rd.bytes()?;
            let weight = rd.f64()?;
            self.collection(&coll)
                .link_weighted(&from, &rel, &to, weight)?;
        }

        Ok(())
    }
}

/// Reject engine-reserved collection names on every dump replay path (audit
/// B8): a dump naming a `__`-prefixed namespace is malformed or hostile — no
/// legitimate dump can contain one, and replaying it would forge
/// engine-internal state.
fn reject_reserved(kind: &str, coll: &str) -> Result<()> {
    if coll.starts_with("__") {
        return Err(Error::InvalidDump(format!(
            "dump contains {kind} for engine-reserved collection '{coll}'"
        )));
    }
    Ok(())
}

/// Collection-name rewriting for [`Db::load_with_renames`] (the `a__b`
/// migration). Every collection-name occurrence in the dump stream passes
/// through [`Renames::apply`] before replay.
///
/// The map is validated upfront, before the stream is touched: every target
/// must pass [`crate::db::validate_name`] (a bad target surfaces that
/// target's own [`Error::InvalidName`]), and no two sources may share a
/// target (both would load into one keyspace and silently overwrite
/// documents — [`Error::InvalidArgument`]).
///
/// `apply` enforces two per-name rules. Engine-reserved (`__`-prefixed)
/// dump names are rejected BEFORE mapping — a rename cannot launder an
/// engine-internal namespace into a user name — and one output name may be
/// produced by at most one dump name per load: a rename target that a
/// second, unmapped dump collection already occupies is the same silent
/// keyspace merge and fails the same way. Names not in the map pass through
/// unchanged (behaving exactly as [`Db::load`]); with an empty map every
/// name maps to itself, so the collision guard can never fire.
struct Renames<'a> {
    map: &'a BTreeMap<String, String>,
    /// Output name → the dump name that first produced it (the streaming
    /// collision guard).
    seen: BTreeMap<String, String>,
}

impl<'a> Renames<'a> {
    fn new(map: &'a BTreeMap<String, String>) -> Result<Self> {
        let mut targets: BTreeMap<&str, &str> = BTreeMap::new();
        for (from, to) in map {
            crate::db::validate_name(to)?;
            if let Some(first) = targets.get(to.as_str()) {
                return Err(Error::InvalidArgument(format!(
                    "rename collision: '{first}' and '{from}' both map to '{to}'"
                )));
            }
            targets.insert(to.as_str(), from.as_str());
        }
        Ok(Renames {
            map,
            seen: BTreeMap::new(),
        })
    }

    /// Reject reserved dump names, then rewrite through the map.
    fn apply(&mut self, kind: &str, name: String) -> Result<String> {
        reject_reserved(kind, &name)?;
        let out = match self.map.get(&name) {
            Some(to) => to.clone(),
            None => name.clone(),
        };
        if let Some(first) = self.seen.get(&out) {
            if *first != name {
                return Err(Error::InvalidArgument(format!(
                    "rename collision: dump collection '{first}' already loads \
                     into '{out}', which '{name}' would also target"
                )));
            }
        } else {
            self.seen.insert(out.clone(), name);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn doc(n: i64) -> Value {
        let mut m = BTreeMap::new();
        m.insert("n".to_owned(), Value::Int(n));
        m.insert("v".to_owned(), Value::Vector(vec![n as f32, 1.0]));
        m.insert("body".to_owned(), Value::Text(format!("item number {n}")));
        Value::Map(m)
    }

    fn seeded() -> Db {
        let db = Db::open_in_memory().unwrap();
        let c = db.collection("docs");
        for i in 0..20i64 {
            c.insert(&[i as u8], &doc(i)).unwrap();
        }
        c.create_scalar_index("n").unwrap();
        c.create_text_index("body").unwrap();
        c.create_compound_index(&["n", "body"]).unwrap();
        c.insert_with_ttl(&[200u8], &doc(200), 12345).unwrap();
        db
    }

    #[test]
    fn dump_load_round_trips_documents_and_defs() {
        use crate::field;
        let src = seeded();
        let mut bytes = Vec::new();
        src.dump(&mut bytes).unwrap();

        // Load into a fresh database.
        let dst = Db::open_in_memory().unwrap();
        dst.load(&bytes[..]).unwrap();
        let c = dst.collection("docs");

        // Documents present (20 + the TTL doc).
        assert_eq!(c.len().unwrap(), 21);
        assert_eq!(c.get(&[7u8]).unwrap(), Some(doc(7)));

        // Scalar index recreated and used.
        let hits: Vec<_> = c
            .query()
            .filter(field("n").eq(Value::Int(5)))
            .run()
            .unwrap()
            .into_iter()
            .map(|r| r.key)
            .collect();
        assert_eq!(hits, vec![vec![5u8]]);

        // Text index recreated.
        assert!(!c.text_search("body", "item", 5).unwrap().is_empty());
        // TTL restored.
        assert_eq!(c.ttl(&[200u8]).unwrap(), Some(12345));
    }

    #[test]
    fn dump_load_through_a_file_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let dump_path = dir.path().join("dump.bin");
        {
            let src = seeded();
            let f = std::fs::File::create(&dump_path).unwrap();
            src.dump(f).unwrap();
        }
        let dst_path = dir.path().join("new.db");
        {
            let dst = Db::open(&dst_path).unwrap();
            let f = std::fs::File::open(&dump_path).unwrap();
            dst.load(f).unwrap();
        }
        // Reopen the freshly loaded DB: data and index defs persisted.
        let dst = Db::open(&dst_path).unwrap();
        assert_eq!(dst.collection("docs").len().unwrap(), 21);
    }

    /// A dump in a FRESH session (reopened, no TTL-touching write since
    /// open) must capture the persisted TTL entries: reopen → dump → load
    /// into a fresh db, and the expiry survives — `ttl()` round-trips and a
    /// purge at the timestamp removes the doc. Pins the enumeration over
    /// persisted `__ttl__*` namespaces end to end.
    #[test]
    fn fresh_session_dump_carries_persisted_ttls() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.db");
        {
            let db = Db::open(&src_path).unwrap();
            db.collection("docs")
                .insert_with_ttl(b"k", &doc(7), 4242)
                .unwrap();
        }
        // Fresh session: reopen, then dump with no TTL write in between.
        let db = Db::open(&src_path).unwrap();
        let mut bytes = Vec::new();
        db.dump(&mut bytes).unwrap();

        let dst = Db::open_in_memory().unwrap();
        dst.load(&bytes[..]).unwrap();
        let c = dst.collection("docs");
        assert_eq!(c.get(b"k").unwrap(), Some(doc(7)));
        assert_eq!(c.ttl(b"k").unwrap(), Some(4242));
        // The restored expiry is honored: a purge at the timestamp removes
        // exactly the one doc.
        assert_eq!(c.purge_expired(4242).unwrap(), 1);
        assert_eq!(c.get(b"k").unwrap(), None);
    }

    #[test]
    fn load_rejects_bad_magic() {
        let dst = Db::open_in_memory().unwrap();
        let err = dst.load(&b"not a corvid dump"[..]);
        assert!(matches!(err, Err(Error::InvalidDump(_))));
    }

    #[test]
    fn empty_db_dump_load() {
        let src = Db::open_in_memory().unwrap();
        let mut bytes = Vec::new();
        src.dump(&mut bytes).unwrap();
        let dst = Db::open_in_memory().unwrap();
        dst.load(&bytes[..]).unwrap();
        assert!(dst.collections().unwrap().is_empty());
    }

    /// Regression: the dump previously dropped every graph edge (they live in
    /// engine-reserved `__edges__*` collections, invisible to
    /// `Db::collections`) and the auto-key counters (so `insert_auto` would
    /// re-issue used keys and silently overwrite documents).
    #[test]
    fn dump_load_preserves_graph_edges_and_auto_ids() {
        let src = Db::open_in_memory().unwrap();
        {
            let c = src.collection("nodes");
            c.insert_auto(&Value::Int(1)).unwrap();
            c.insert_auto(&Value::Int(2)).unwrap();
            c.link_weighted(b"a", "knows", b"b", 0.75).unwrap();
            c.link(b"b", "knows", b"c").unwrap();
        }
        let mut bytes = Vec::new();
        src.dump(&mut bytes).unwrap();

        let dst = Db::open_in_memory().unwrap();
        dst.load(&bytes[..]).unwrap();
        let c = dst.collection("nodes");
        // Edges survive, both directions, with weights.
        assert_eq!(c.neighbors(b"a", "knows").unwrap(), vec![b"b".to_vec()]);
        assert_eq!(c.in_neighbors(b"b", "knows").unwrap(), vec![b"a".to_vec()]);
        assert_eq!(
            c.neighbors_weighted(b"a", "knows").unwrap(),
            vec![(b"b".to_vec(), 0.75)]
        );
        // Unweighted edges read back as 1.0.
        assert_eq!(
            c.neighbors_weighted(b"b", "knows").unwrap(),
            vec![(b"c".to_vec(), 1.0)]
        );
        // The auto-id counter continues past the pre-dump keys.
        let next = c.insert_auto(&Value::Int(3)).unwrap();
        assert_eq!(next, b"00000000000000000002".to_vec());
        // And it did not overwrite an existing document.
        assert_eq!(
            c.get(&b"00000000000000000001"[..]).unwrap(),
            Some(Value::Int(2))
        );
    }

    /// A dump naming an engine-reserved collection is rejected instead of
    /// forging internal state.
    #[test]
    fn load_rejects_reserved_collection_names() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        put_u64(&mut bytes, 1); // one record
        put_str(&mut bytes, "__schemas__"); // reserved name
        put_bytes(&mut bytes, b"users");
        put_bytes(&mut bytes, &Vec::new()); // empty value
        let dst = Db::open_in_memory().unwrap();
        let err = dst.load(&bytes[..]);
        assert!(matches!(err, Err(Error::InvalidDump(msg)) if msg.contains("reserved")));
    }

    /// A `Read` that delivers at most 3 bytes per call (audit B8): the
    /// streaming load must reassemble every field across arbitrarily
    /// fragmented delivery, never assuming one `read` fills the request.
    struct Trickle<R> {
        inner: R,
    }
    impl<R: Read> Read for Trickle<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(3);
            self.inner.read(&mut buf[..n])
        }
    }

    /// The dump's record+counter sections must come from ONE read snapshot
    /// (audit B8): while a writer commits auto-keyed documents, a dump taken
    /// concurrently can only ever capture a gap-free PREFIX of its inserts
    /// with an auto-id counter that exactly matches them — reading records
    /// and counters in separate transactions could capture a counter ahead
    /// of the documents it named, which `load` would then honor by skipping
    /// an id (or, torn the other way, re-issuing one).
    #[test]
    fn dump_is_one_snapshot_under_a_concurrent_writer() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let src = Db::open_in_memory().unwrap();
        {
            let c = src.collection("seed");
            for i in 0..3000i64 {
                c.insert(&i.to_be_bytes(), &Value::Int(i)).unwrap();
            }
        }
        let db = Arc::new(src);
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let db = Arc::clone(&db);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let c = db.collection("auto");
                let mut i = 0i64;
                while !stop.load(Ordering::Relaxed) {
                    c.insert_auto(&Value::Int(i)).unwrap();
                    i += 1;
                }
            })
        };
        let mut bytes = Vec::new();
        db.dump(&mut bytes).unwrap();
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();

        let dst = Db::open_in_memory().unwrap();
        dst.load(&bytes[..]).unwrap();
        let c = dst.collection("auto");
        let docs = c.scan().unwrap();
        // Zero-padded 20-wide decimal keys sort numerically, so a gap-free
        // prefix is exactly ids 0..k in scan order.
        for (idx, (key, _)) in docs.iter().enumerate() {
            assert_eq!(
                key,
                format!("{idx:020}").as_bytes(),
                "auto ids in the dump must be a gap-free prefix"
            );
        }
        // The restored counter equals the prefix length: it came from the
        // same snapshot as the records (a torn dump restores it ahead).
        let k = docs.len();
        let next = c.insert_auto(&Value::Int(-1)).unwrap();
        assert_eq!(next, format!("{k:020}").as_bytes());
        // The static seed survived intact.
        assert_eq!(dst.collection("seed").len().unwrap(), 3000);
    }

    /// Audit B8: reserved-name rejection on EVERY replay path — the
    /// vector/text/scalar/geo index sections and schemas join records,
    /// compound, TTL, and edges. A dump naming an engine-reserved collection
    /// in any section is malformed or hostile.
    #[test]
    fn load_rejects_reserved_collection_on_index_and_schema_paths() {
        // A valid dump prologue with zero records.
        fn head() -> Vec<u8> {
            let mut b = Vec::new();
            b.extend_from_slice(MAGIC);
            put_u64(&mut b, 0);
            b
        }
        let reserved = "__schemas__";
        let cases: Vec<(&str, Vec<u8>)> = vec![
            // One vector index def on a reserved collection.
            {
                let mut b = head();
                put_u64(&mut b, 1);
                put_str(&mut b, reserved);
                put_str(&mut b, "f");
                b.push(0); // metric
                b.push(0); // quant
                b.push(0); // mode: in-memory
                ("vector", b)
            },
            // One text index def.
            {
                let mut b = head();
                put_u64(&mut b, 0); // vectors
                put_u64(&mut b, 1);
                put_str(&mut b, reserved);
                put_str(&mut b, "f");
                b.push(0);
                ("text", b)
            },
            // One scalar index def.
            {
                let mut b = head();
                put_u64(&mut b, 0); // vectors
                put_u64(&mut b, 0); // texts
                put_u64(&mut b, 1);
                put_str(&mut b, reserved);
                put_str(&mut b, "f");
                ("scalar", b)
            },
            // One geo index def.
            {
                let mut b = head();
                put_u64(&mut b, 0); // vectors
                put_u64(&mut b, 0); // texts
                put_u64(&mut b, 0); // scalars
                put_u64(&mut b, 0); // compounds
                put_u64(&mut b, 1);
                put_str(&mut b, reserved);
                put_str(&mut b, "f");
                ("geo", b)
            },
            // One schema.
            {
                let mut b = head();
                put_u64(&mut b, 0); // vectors
                put_u64(&mut b, 0); // texts
                put_u64(&mut b, 0); // scalars
                put_u64(&mut b, 0); // compounds
                put_u64(&mut b, 0); // geos
                put_u64(&mut b, 1);
                put_str(&mut b, reserved);
                put_u32(&mut b, 1); // one field
                b.push(2); // FieldType::Int
                b.push(0); // required
                b.push(0); // unique
                put_str(&mut b, "f");
                ("schema", b)
            },
        ];
        for (kind, bytes) in cases {
            let dst = Db::open_in_memory().unwrap();
            let err = dst.load(&bytes[..]);
            assert!(
                matches!(&err, Err(Error::InvalidDump(msg)) if msg.contains("reserved")),
                "{kind} index/schema on a reserved collection must be rejected as InvalidDump, got {err:?}"
            );
        }
    }

    /// Comprehensive dump→load parity on a rich database (audit B8):
    /// documents across collections, every index kind, a schema, TTLs, graph
    /// edges, and auto counters all survive — and the recreated indexes stay
    /// SERVICEABLE, not just present.
    #[test]
    fn dump_load_parity_on_a_rich_database() {
        use crate::field;
        use crate::schema::{Field, FieldType, Schema};
        let src = Db::open_in_memory().unwrap();
        {
            let c = src.collection("docs");
            for i in 0..5i64 {
                c.insert(&[i as u8], &doc(i)).unwrap();
            }
            c.create_scalar_index("n").unwrap();
            c.create_text_index("body").unwrap();
            c.create_compound_index(&["n", "body"]).unwrap();
            c.create_vector_index("v", Metric::Cosine).unwrap();
            c.link_weighted(b"a", "knows", b"b", 0.5).unwrap();
            c.link(b"b", "knows", b"c").unwrap();
            c.insert_auto(&Value::Int(1)).unwrap();
            c.insert_auto(&Value::Int(2)).unwrap();
            c.insert_with_ttl(&[250u8], &doc(250), 424242).unwrap();
            let s = Schema::new()
                .field(Field::new("n", FieldType::Int).required())
                .field(Field::new("body", FieldType::Text));
            c.set_schema(&s).unwrap();
            let o = src.collection("other");
            o.insert(b"z", &Value::Text("hi".into())).unwrap();
        }
        let mut bytes = Vec::new();
        src.dump(&mut bytes).unwrap();
        let dst = Db::open_in_memory().unwrap();
        dst.load(&bytes[..]).unwrap();
        let c = dst.collection("docs");

        // Documents: 5 plain + 2 auto + 1 ttl.
        assert_eq!(c.len().unwrap(), 8);
        assert_eq!(c.get(&[7u8]).unwrap(), None); // not seeded
        assert_eq!(c.get(b"00000000000000000000").unwrap(), Some(Value::Int(1)));

        // Scalar index recreated AND serviceable.
        let hits: Vec<_> = c
            .query()
            .filter(field("n").eq(Value::Int(3)))
            .run()
            .unwrap()
            .into_iter()
            .map(|r| r.key)
            .collect();
        assert_eq!(hits, vec![vec![3u8]]);

        // Text index recreated.
        assert!(!c.text_search("body", "item", 5).unwrap().is_empty());

        // Vector index recreated and serving.
        let vhits = c
            .query()
            .vector("v", vec![2.0, 1.0], 3, Metric::Cosine)
            .run()
            .unwrap();
        assert_eq!(vhits.len(), 3);
        assert_eq!(vhits[0].key, vec![2u8]); // closest to the query

        // TTL restored.
        assert_eq!(c.ttl(&[250u8]).unwrap(), Some(424242));
        // Edges restored, both directions, weights intact.
        assert_eq!(c.neighbors(b"a", "knows").unwrap(), vec![b"b".to_vec()]);
        assert_eq!(c.in_neighbors(b"b", "knows").unwrap(), vec![b"a".to_vec()]);
        assert_eq!(
            c.neighbors_weighted(b"a", "knows").unwrap(),
            vec![(b"b".to_vec(), 0.5)]
        );
        // Auto counters: next insert continues past the dumped ids.
        assert_eq!(
            c.insert_auto(&doc(9)).unwrap(),
            b"00000000000000000002".to_vec()
        );
        // Schema restored and enforced on new writes.
        let mut bad = BTreeMap::new();
        bad.insert("body".to_owned(), Value::Text("no n".into()));
        assert!(matches!(
            c.insert(b"new", &Value::Map(bad)),
            Err(Error::SchemaViolation(_))
        ));
        // Second collection came through.
        assert_eq!(
            dst.collection("other").get(b"z").unwrap(),
            Some(Value::Text("hi".into()))
        );
    }

    /// Audit B8: load is streaming — fields must survive arbitrarily
    /// fragmented `Read` delivery (at most 3 bytes per call here), as when
    /// loading from a slow socket or pipe rather than a memory buffer.
    #[test]
    fn load_handles_fragmented_stream_delivery() {
        let src = seeded();
        let mut bytes = Vec::new();
        src.dump(&mut bytes).unwrap();
        let dst = Db::open_in_memory().unwrap();
        dst.load(Trickle { inner: &bytes[..] }).unwrap();
        assert_eq!(dst.collection("docs").len().unwrap(), 21);
    }

    /// An absurd field count must not drive a huge allocation.
    #[test]
    fn load_rejects_absurd_compound_field_counts() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        put_u64(&mut bytes, 0); // no records
        put_u64(&mut bytes, 0); // vector indexes
        put_u64(&mut bytes, 0); // text indexes
        put_u64(&mut bytes, 0); // scalar indexes
        put_u64(&mut bytes, 1); // compound indexes
        put_str(&mut bytes, "docs");
        put_u32(&mut bytes, u32::MAX as usize); // absurd field count
        let dst = Db::open_in_memory().unwrap();
        // Must fail cleanly on truncated input, not abort on allocation.
        assert!(matches!(dst.load(&bytes[..]), Err(Error::InvalidDump(_))));
    }

    /// Audit B8 family gap (found in Task 8): the auto-id section was the
    /// ONE replay path without `reject_reserved` — a dump naming an
    /// engine-reserved collection there would forge a `auto:__…` META
    /// counter. Must be `InvalidDump` like every other section. (RED first:
    /// passed Ok before the fix.)
    #[test]
    fn load_rejects_reserved_collection_on_auto_id_path() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        put_u64(&mut bytes, 0); // records
        put_u64(&mut bytes, 0); // vectors
        put_u64(&mut bytes, 0); // texts
        put_u64(&mut bytes, 0); // scalars
        put_u64(&mut bytes, 0); // compounds
        put_u64(&mut bytes, 0); // geos
        put_u64(&mut bytes, 0); // schemas
        put_u64(&mut bytes, 0); // ttls
        put_u64(&mut bytes, 1); // one auto-id counter...
        put_str(&mut bytes, "__edges__docs"); // ...for a reserved namespace
        bytes.extend_from_slice(&5u64.to_le_bytes());
        put_u64(&mut bytes, 0); // edges
        let dst = Db::open_in_memory().unwrap();
        let err = dst.load(&bytes[..]);
        assert!(
            matches!(&err, Err(Error::InvalidDump(msg)) if msg.contains("reserved")),
            "reserved auto-id collection must be rejected as InvalidDump, got {err:?}"
        );
    }

    /// A compact legacy-dump rename: a dump naming `a__b` (impossible to
    /// produce with the current `dump`, since name validation rejects it)
    /// loads under a fresh name with its records, scalar index, TTL, edge,
    /// and auto-id counter all moved together. (The full-family conformance
    /// pin lives in tests/lifecycle.rs.)
    #[test]
    fn load_with_renames_migrates_a_legacy_collection() {
        let doc = {
            let mut m = BTreeMap::new();
            m.insert("n".to_owned(), Value::Int(7));
            Value::Map(m)
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        put_u64(&mut bytes, 1); // one record
        put_str(&mut bytes, "a__b");
        put_bytes(&mut bytes, b"k");
        put_bytes(&mut bytes, &doc.encode());
        put_u64(&mut bytes, 0); // vectors
        put_u64(&mut bytes, 0); // texts
        put_u64(&mut bytes, 1); // one scalar index on (a__b, n)
        put_str(&mut bytes, "a__b");
        put_str(&mut bytes, "n");
        put_u64(&mut bytes, 0); // compounds
        put_u64(&mut bytes, 0); // geos
        put_u64(&mut bytes, 0); // schemas
        put_u64(&mut bytes, 1); // one TTL on (a__b, k)
        put_str(&mut bytes, "a__b");
        put_bytes(&mut bytes, b"k");
        put_i64(&mut bytes, 4242);
        put_u64(&mut bytes, 1); // one auto-id counter for a__b
        put_str(&mut bytes, "a__b");
        bytes.extend_from_slice(&2u64.to_le_bytes());
        put_u64(&mut bytes, 1); // one edge on (a__b)
        put_str(&mut bytes, "a__b");
        put_str(&mut bytes, "r");
        put_bytes(&mut bytes, b"a");
        put_bytes(&mut bytes, b"b");
        put_f64(&mut bytes, 0.5);

        let mut renames = BTreeMap::new();
        renames.insert("a__b".to_owned(), "a_b".to_owned());
        let dst = Db::open_in_memory().unwrap();
        dst.load_with_renames(&bytes[..], &renames).unwrap();

        // Everything landed under the new name; the old name is gone.
        assert_eq!(dst.collections().unwrap(), vec!["a_b".to_owned()]);
        let c = dst.collection("a_b");
        assert_eq!(c.get(b"k").unwrap(), Some(doc));
        // The scalar index was created under the renamed collection and is
        // serviceable: the equality filter resolves through it.
        let hits: Vec<_> = c
            .query()
            .filter(crate::field("n").eq(Value::Int(7)))
            .run()
            .unwrap()
            .into_iter()
            .map(|r| r.key)
            .collect();
        assert_eq!(hits, vec![b"k".to_vec()]);
        assert_eq!(c.ttl(b"k").unwrap(), Some(4242));
        assert_eq!(c.neighbors(b"a", "r").unwrap(), vec![b"b".to_vec()]);
        // The restored counter (2) continues under the new name.
        assert_eq!(
            c.insert_auto(&Value::Int(1)).unwrap(),
            b"00000000000000000002".to_vec()
        );
    }

    /// The rename-map contract: an invalid target is that target's
    /// `InvalidName`; two sources sharing a target, and a target colliding
    /// with an unmapped dump collection, are `InvalidArgument` (one output
    /// keyspace per dump name); a reserved dump name cannot be laundered by
    /// a rename; an absent source is a no-op.
    #[test]
    fn load_with_renames_error_contract() {
        // One record under a__b, one under a_b.
        fn two_records() -> Vec<u8> {
            let mut b = Vec::new();
            b.extend_from_slice(MAGIC);
            put_u64(&mut b, 2);
            put_str(&mut b, "a__b");
            put_bytes(&mut b, b"x");
            put_bytes(&mut b, &Value::Int(1).encode());
            put_str(&mut b, "a_b");
            put_bytes(&mut b, b"y");
            put_bytes(&mut b, &Value::Int(2).encode());
            b
        }
        let map = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect()
        };

        // Invalid target: the offending rename's own InvalidName.
        let dst = Db::open_in_memory().unwrap();
        let err = dst.load_with_renames(&two_records()[..], &map(&[("a__b", "x__y")]));
        assert!(
            matches!(&err, Err(Error::InvalidName(n)) if n == "x__y"),
            "invalid target must be InvalidName naming it, got {err:?}"
        );

        // Two sources sharing one target.
        let err = Db::open_in_memory()
            .unwrap()
            .load_with_renames(&two_records()[..], &map(&[("a__b", "z"), ("c__d", "z")]));
        assert!(
            matches!(&err, Err(Error::InvalidArgument(m)) if m.contains("a__b") && m.contains("c__d")),
            "shared target must be InvalidArgument naming both sources, got {err:?}"
        );

        // A target colliding with an UNMAPPED dump collection (a__b → a_b
        // while the dump also carries a_b) is the same keyspace merge.
        let err = Db::open_in_memory()
            .unwrap()
            .load_with_renames(&two_records()[..], &map(&[("a__b", "a_b")]));
        assert!(
            matches!(&err, Err(Error::InvalidArgument(m)) if m.contains("a_b")),
            "dump-vs-map collision must be InvalidArgument, got {err:?}"
        );

        // A reserved dump name is rejected before mapping — no laundering.
        let mut reserved = Vec::new();
        reserved.extend_from_slice(MAGIC);
        put_u64(&mut reserved, 1);
        put_str(&mut reserved, "__edges__docs");
        put_bytes(&mut reserved, b"k");
        put_bytes(&mut reserved, &Value::Int(1).encode());
        let err = Db::open_in_memory()
            .unwrap()
            .load_with_renames(&reserved[..], &map(&[("__edges__docs", "laundry")]));
        assert!(
            matches!(&err, Err(Error::InvalidDump(m)) if m.contains("reserved")),
            "a rename must not launder a reserved dump name, got {err:?}"
        );

        // A map entry whose source never occurs in the dump is a no-op.
        let mut dump = two_records();
        put_u64(&mut dump, 0); // vectors
        put_u64(&mut dump, 0); // texts
        put_u64(&mut dump, 0); // scalars
        put_u64(&mut dump, 0); // compounds
        put_u64(&mut dump, 0); // geos
        put_u64(&mut dump, 0); // schemas
        put_u64(&mut dump, 0); // ttls
        put_u64(&mut dump, 0); // autos
        put_u64(&mut dump, 0); // edges
        let dst = Db::open_in_memory().unwrap();
        dst.load_with_renames(&dump[..], &map(&[("absent__name", "zzz")]))
            .unwrap();
        assert_eq!(
            dst.collections().unwrap(),
            vec!["a__b".to_owned(), "a_b".to_owned()]
        );
    }
}
