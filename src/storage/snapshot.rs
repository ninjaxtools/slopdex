//! Canonical snapshot manifest and streamed digest.

use super::*;

/// One section of the canonical snapshot document, in serialized key order.
enum SnapshotSection {
    Rows(&'static str),
    ParseBindings,
    Schema,
}

/// Sections are listed in the bytewise key order of the canonical JSON object.
/// The order, the queries and their column order are part of the digest contract:
/// changing any of them changes every `metadata.snapshot` and result-cache key.
const SNAPSHOT_SECTIONS: [(&str, SnapshotSection); 10] = [
    (
        "descriptions",
        SnapshotSection::Rows(
            "SELECT scope,path,identity,source_hash,content_hash,embedding_key FROM descriptions ORDER BY path,scope,identity",
        ),
    ),
    (
        "diagnostics",
        SnapshotSection::Rows("SELECT path,ordinal,data FROM diagnostics ORDER BY path,ordinal"),
    ),
    (
        "embeddings",
        SnapshotSection::Rows(
            "SELECT s.path,s.identity,e.role,e.profile_key,e.input_hash,e.embedding_key FROM unit_embeddings e JOIN search_units s ON s.id=e.unit_id ORDER BY s.path,s.identity,e.role,e.profile_key,e.input_hash",
        ),
    ),
    (
        "files",
        SnapshotSection::Rows(
            "SELECT path,hash,language,source_mode,parser_version,structure_hash FROM files ORDER BY path",
        ),
    ),
    (
        "names",
        SnapshotSection::Rows(
            "SELECT path,symbol_id,ordinal,name FROM symbol_names ORDER BY path,symbol_id,ordinal",
        ),
    ),
    // These parser bindings identify reusable extraction independent of paths.
    ("parseBindings", SnapshotSection::ParseBindings),
    (
        "policy",
        SnapshotSection::Rows(
            "SELECT key,value FROM metadata WHERE key='selection_policy' ORDER BY key",
        ),
    ),
    ("schema", SnapshotSection::Schema),
    (
        "symbols",
        SnapshotSection::Rows(
            "SELECT path,id,parent_id,ordinal,language,kind,name,qualified_name,signature,start_byte,end_byte,start_line,start_column,end_line,end_column,metadata FROM symbols ORDER BY path,ordinal",
        ),
    ),
    (
        "units",
        SnapshotSection::Rows(
            "SELECT path,identity,kind,symbol_id,data,embedding_input_hash,content_hash FROM search_units ORDER BY path,identity",
        ),
    ),
];

struct HashWriter(sha2::Sha256);

impl std::io::Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        sha2::Digest::update(&mut self.0, bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The digest excludes workspace root, row IDs, generation and git checkpoint.
/// It includes every normalized binding, parser contract and selection/provenance
/// policy, so identical published content has the same digest. The canonical
/// document is hashed while it is produced; it is never held in memory or stored.
pub(super) fn snapshot_digest(conn: &Connection) -> Result<String> {
    let mut out = std::io::BufWriter::with_capacity(64 * 1024, HashWriter(Default::default()));
    write_snapshot_manifest(conn, &mut out)?;
    let hasher = out
        .into_inner()
        .map_err(|error| error.into_error())
        .context("Hash snapshot manifest")?;
    Ok(format!("{:x}", sha2::Digest::finalize(hasher.0)))
}

/// Emit exactly the bytes `serde_json::to_string` produces for the canonical
/// object: compact separators, keys in bytewise order, and serde_json's own
/// scalar serializers for every value so escaping and integers are identical.
pub(super) fn write_snapshot_manifest(
    conn: &Connection,
    out: &mut impl std::io::Write,
) -> Result<()> {
    debug_assert!(SNAPSHOT_SECTIONS.is_sorted_by_key(|(name, _)| *name));
    out.write_all(b"{")?;
    for (index, (name, section)) in SNAPSHOT_SECTIONS.iter().enumerate() {
        if index > 0 {
            out.write_all(b",")?;
        }
        serde_json::to_writer(&mut *out, name)?;
        out.write_all(b":")?;
        match section {
            SnapshotSection::Schema => out.write_all(b"1")?,
            SnapshotSection::Rows(sql) => {
                let mut stmt = conn.prepare(sql)?;
                let columns = stmt.column_count();
                let mut rows = stmt.query([])?;
                out.write_all(b"[")?;
                let mut first = true;
                while let Some(row) = rows.next()? {
                    if !first {
                        out.write_all(b",")?;
                    }
                    out.write_all(b"[")?;
                    first = false;
                    for column in 0..columns {
                        if column > 0 {
                            out.write_all(b",")?;
                        }
                        match row.get_ref(column)? {
                            rusqlite::types::ValueRef::Null => out.write_all(b"null")?,
                            rusqlite::types::ValueRef::Integer(value) => {
                                serde_json::to_writer(&mut *out, &value)?
                            }
                            rusqlite::types::ValueRef::Text(value) => {
                                serde_json::to_writer(&mut *out, &*String::from_utf8_lossy(value))?
                            }
                            other => {
                                return Err(rusqlite::Error::InvalidColumnType(
                                    column,
                                    (*name).into(),
                                    other.data_type(),
                                )
                                .into());
                            }
                        }
                    }
                    out.write_all(b"]")?;
                }
                out.write_all(b"]")?;
            }
            SnapshotSection::ParseBindings => {
                let mut stmt = conn.prepare("SELECT DISTINCT hash,language,parser_version FROM files WHERE parser_version IS NOT NULL AND structure_hash=hash ORDER BY hash,language,parser_version")?;
                let mut rows = stmt.query([])?;
                out.write_all(b"[")?;
                let mut first = true;
                while let Some(row) = rows.next()? {
                    let (hash, language, version): (String, String, String) =
                        (row.get(0)?, row.get(1)?, row.get(2)?);
                    if !first {
                        out.write_all(b",")?;
                    }
                    out.write_all(b"{")?;
                    first = false;
                    for (index, (key, value)) in [
                        ("language", language),
                        ("parserVersion", version),
                        ("sourceHash", hash),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        if index > 0 {
                            out.write_all(b",")?;
                        }
                        serde_json::to_writer(&mut *out, key)?;
                        out.write_all(b":")?;
                        serde_json::to_writer(&mut *out, &value)?;
                    }
                    out.write_all(b"}")?;
                }
                out.write_all(b"]")?;
            }
        }
    }
    out.write_all(b"}")?;
    Ok(())
}

/// The former materializing implementation, kept as the byte-level reference
/// that the streamed digest must reproduce.
#[cfg(test)]
pub(super) fn snapshot_manifest(conn: &Connection) -> Result<String> {
    let mut manifest = serde_json::Map::new();
    manifest.insert("schema".into(), json!(1));
    for (name, sql) in [
        (
            "files",
            "SELECT path,hash,language,source_mode,parser_version,structure_hash FROM files ORDER BY path",
        ),
        (
            "symbols",
            "SELECT path,id,parent_id,ordinal,language,kind,name,qualified_name,signature,start_byte,end_byte,start_line,start_column,end_line,end_column,metadata FROM symbols ORDER BY path,ordinal",
        ),
        (
            "names",
            "SELECT path,symbol_id,ordinal,name FROM symbol_names ORDER BY path,symbol_id,ordinal",
        ),
        (
            "units",
            "SELECT path,identity,kind,symbol_id,data,embedding_input_hash,content_hash FROM search_units ORDER BY path,identity",
        ),
        (
            "embeddings",
            "SELECT s.path,s.identity,e.role,e.profile_key,e.input_hash,e.embedding_key FROM unit_embeddings e JOIN search_units s ON s.id=e.unit_id ORDER BY s.path,s.identity,e.role,e.profile_key,e.input_hash",
        ),
        (
            "descriptions",
            "SELECT scope,path,identity,source_hash,content_hash,embedding_key FROM descriptions ORDER BY path,scope,identity",
        ),
        (
            "diagnostics",
            "SELECT path,ordinal,data FROM diagnostics ORDER BY path,ordinal",
        ),
        (
            "policy",
            "SELECT key,value FROM metadata WHERE key='selection_policy' ORDER BY key",
        ),
    ] {
        let mut stmt = conn.prepare(sql)?;
        let columns = stmt.column_count();
        let rows = stmt
            .query_map([], |r| {
                (0..columns)
                    .map(|i| {
                        Ok(match r.get_ref(i)? {
                            rusqlite::types::ValueRef::Null => Value::Null,
                            rusqlite::types::ValueRef::Integer(v) => json!(v),
                            rusqlite::types::ValueRef::Text(v) => json!(String::from_utf8_lossy(v)),
                            _ => {
                                return Err(rusqlite::Error::InvalidColumnType(
                                    i,
                                    name.into(),
                                    r.get_ref(i)?.data_type(),
                                ));
                            }
                        })
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        manifest.insert(name.into(), json!(rows));
    }
    // These parser bindings identify reusable extraction independent of paths.
    let bindings = conn.prepare("SELECT DISTINCT hash,language,parser_version FROM files WHERE parser_version IS NOT NULL AND structure_hash=hash ORDER BY hash,language,parser_version")?
        .query_map([], |r| Ok(json!({"sourceHash": r.get::<_, String>(0)?, "language": r.get::<_, String>(1)?, "parserVersion": r.get::<_, String>(2)?})))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    manifest.insert("parseBindings".into(), json!(bindings));
    Ok(serde_json::to_string(&manifest)?)
}
