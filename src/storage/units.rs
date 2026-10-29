//! Search-unit content, structures, and record read/write helpers.

use super::publish::*;
use super::*;

type Description = (Option<String>, String, Option<String>);

pub(super) fn unit_payload(data: &Value) -> Result<String> {
    ensure!(data.is_object(), "Invalid stored search unit");
    let mut content = serde_json::Map::new();
    for field in ["source", "embeddingInput"] {
        if let Some(value) = data.get(field) {
            content.insert(field.into(), value.clone());
        }
    }
    Ok(serde_json::to_string(&content)?)
}

#[cfg(test)]
pub(super) fn unit_content(conn: &Connection, key: &str) -> Result<serde_json::Map<String, Value>> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT value FROM global.cache WHERE kind='unit-content' AND key=?",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    decode_unit_content(key, payload)
}

pub(super) fn decode_unit_content(
    key: &str,
    payload: Option<String>,
) -> Result<serde_json::Map<String, Value>> {
    let payload = payload.with_context(|| {
        format!("Missing global unit content artifact {key}; rebuild the index")
    })?;
    ensure!(
        hash(&payload) == key,
        "Corrupt global unit content artifact {key}: content hash mismatch"
    );
    let content: Value = serde_json::from_str(&payload)
        .with_context(|| format!("Invalid global unit content artifact {key}"))?;
    let Value::Object(content) = content else {
        bail!("Invalid global unit content artifact {key}: expected an object");
    };
    ensure!(
        content
            .keys()
            .all(|field| matches!(field.as_str(), "source" | "embeddingInput")),
        "Invalid global unit content artifact {key}: unexpected fields"
    );
    Ok(content)
}

/// A binding whose interned text is absent is a damaged store, not "no
/// description": report which artifact is missing instead of a column type error.
pub(super) fn description_text(content_hash: &str, content: Option<String>) -> Result<String> {
    content.with_context(|| {
        format!("Missing global description content {content_hash}; rebuild the index")
    })
}

pub(super) fn description(
    conn: &Connection,
    scope: &str,
    path: &str,
    identity: &str,
) -> Result<Option<Description>> {
    let row = conn
        .prepare_cached("SELECT d.source_hash,d.content_hash,c.content,d.embedding_key FROM descriptions d LEFT JOIN global.description_content c ON c.hash=d.content_hash WHERE d.scope=? AND d.path=? AND d.identity=?")?
        .query_row(params![scope, path, identity], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .optional()?;
    row.map(|(source_hash, content_hash, content, embedding)| {
        Ok((
            source_hash,
            description_text(&content_hash, content)?,
            embedding,
        ))
    })
    .transpose()
}

pub(super) fn latest_embedding(
    conn: &Connection,
    id: u64,
    role: &str,
    input_hash: &str,
) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached("SELECT embedding_key FROM unit_embeddings WHERE unit_id=? AND role=? AND input_hash=? ORDER BY rowid DESC LIMIT 1")?
        .query_row(params![i64::try_from(id)?, role, input_hash], |r| r.get(0))
        .optional()?)
}

pub(super) fn write_file(conn: &Connection, file: &File, structural: bool) -> Result<()> {
    let mut file = file.clone();
    if !structural
        && !file
            .description_hash
            .as_deref()
            .is_some_and(|hash| hash.starts_with("source:"))
        && conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE path=? AND hash=?)",
            params![file.path, file.hash],
            |row| row.get::<_, bool>(0),
        )?
        && let Some((source_hash, text, embedding)) = description(conn, "file", &file.path, "")?
        && source_hash
            .as_deref()
            .is_some_and(|hash| hash.starts_with("source:"))
    {
        if file.description.as_ref() != Some(&text) {
            file.description_embedding = embedding;
        }
        file.description = Some(text);
        file.description_hash = source_hash;
    }
    ensure!(
        hash(&file.source) == file.hash,
        "Source content hash mismatch for {}",
        file.path
    );
    ensure!(
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM global.sources WHERE hash=? AND source=?)",
            params![file.hash, file.source],
            |row| row.get::<_, bool>(0)
        )?,
        "Missing or corrupt global source artifact for {} ({})",
        file.path,
        file.hash
    );
    conn.execute("INSERT INTO files(path,hash,language,source_mode) VALUES(?,?,?,?) ON CONFLICT(path) DO UPDATE SET
        hash=excluded.hash,language=excluded.language,source_mode=excluded.source_mode,
        parser_version=CASE WHEN files.hash=excluded.hash THEN files.parser_version END,
        structure_hash=CASE WHEN files.hash=excluded.hash THEN files.structure_hash END",
            params![file.path, file.hash, file.language, file.source_mode])?;
    conn.execute(
        "DELETE FROM descriptions WHERE scope='file' AND path=?",
        [&file.path],
    )?;
    if let Some(text) = &file.description {
        let content_hash = description_reference(conn, text)?;
        validate_embedding_reference(conn, file.description_embedding.as_deref())?;
        conn.execute("INSERT INTO descriptions(scope,path,identity,source_hash,content_hash,embedding_key) VALUES('file',?,'',?,?,?)",
            params![file.path, file.description_hash, content_hash, file.description_embedding])?;
    }
    conn.execute("DELETE FROM diagnostics WHERE path=?", [&file.path])?;
    for (ordinal, error) in file.errors.iter().enumerate() {
        conn.execute("INSERT INTO diagnostics(path,ordinal,code,message,start_line,end_line,data) VALUES(?,?,?,?,?,?,?)",
            params![file.path, i64::try_from(ordinal)?, error["code"].as_str(), error["message"].as_str(),
                error["startLine"].as_i64(), error["endLine"].as_i64(), error.to_string()])?;
    }
    Ok(())
}

pub(super) fn write_structure(
    conn: &Connection,
    path: &str,
    structure: &FileStructure,
) -> Result<()> {
    for (ordinal, node) in structure.nodes.iter().enumerate() {
        let description_hash = node
            .description
            .as_deref()
            .map(|text| description_reference(conn, text))
            .transpose()?;
        let metadata = json!({"attributes":node.attributes,"imports":node.imports,"headingLevel":node.heading_level,"calls":node.calls,"references":node.references,"descriptionHash":description_hash,"declarationStartLine":node.declaration_start_line,"declarationStartByte":node.declaration_start_byte});
        conn.execute("INSERT INTO symbols(path,id,parent_id,ordinal,language,kind,name,qualified_name,signature,start_byte,end_byte,start_line,start_column,end_line,end_column,metadata) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![path, i64::try_from(node.id)?, node.parent_id.map(i64::try_from).transpose()?, i64::try_from(ordinal)?, node.language, node.kind, node.name, node.qualified_name,
                node.signature, i64::try_from(node.start_byte)?, i64::try_from(node.end_byte)?, i64::try_from(node.start_line)?, i64::try_from(node.start_column)?, i64::try_from(node.end_line)?, i64::try_from(node.end_column)?, metadata.to_string()])?;
        for (ordinal, name) in node.names.iter().enumerate() {
            conn.execute(
                "INSERT INTO symbol_names(path,symbol_id,ordinal,name) VALUES(?,?,?,?)",
                params![path, i64::try_from(node.id)?, i64::try_from(ordinal)?, name],
            )?;
        }
    }
    Ok(())
}

pub(super) fn row_usize(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<usize> {
    let value: i64 = row.get(column)?;
    usize::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(column, value))
}

pub(super) fn read_structure(conn: &Connection, path: &str) -> Result<FileStructure> {
    let mut names = HashMap::<usize, Vec<String>>::new();
    let mut names_stmt = conn.prepare(
        "SELECT symbol_id,name FROM symbol_names WHERE path=? ORDER BY symbol_id,ordinal",
    )?;
    for row in names_stmt.query_map([path], |r| Ok((row_usize(r, 0)?, r.get::<_, String>(1)?)))? {
        let (id, name) = row?;
        names.entry(id).or_default().push(name);
    }
    let mut stmt = conn.prepare("SELECT id,parent_id,language,kind,name,qualified_name,signature,start_byte,end_byte,start_line,start_column,end_line,end_column,metadata FROM symbols WHERE path=? ORDER BY ordinal")?;
    let rows = stmt.query_map([path], |r| {
        Ok((
            StructureNode {
                id: row_usize(r, 0)?,
                parent_id: r
                    .get::<_, Option<i64>>(1)?
                    .map(|_| row_usize(r, 1))
                    .transpose()?,
                language: r.get(2)?,
                kind: r.get(3)?,
                name: r.get(4)?,
                qualified_name: r.get(5)?,
                signature: r.get(6)?,
                start_byte: row_usize(r, 7)?,
                end_byte: row_usize(r, 8)?,
                start_line: row_usize(r, 9)?,
                start_column: row_usize(r, 10)?,
                end_line: row_usize(r, 11)?,
                end_column: row_usize(r, 12)?,
                ..Default::default()
            },
            r.get::<_, String>(13)?,
        ))
    })?;
    let nodes = rows
        .map(|r| {
            let (mut node, metadata) = r?;
            let metadata: Value = serde_json::from_str(&metadata)?;
            node.attributes = serde_json::from_value(metadata["attributes"].clone())?;
            node.imports = serde_json::from_value(metadata["imports"].clone())?;
            node.heading_level = serde_json::from_value(metadata["headingLevel"].clone())?;
            node.declaration_start_line =
                serde_json::from_value(metadata["declarationStartLine"].clone())?;
            node.declaration_start_byte =
                serde_json::from_value(metadata["declarationStartByte"].clone())?;
            node.calls =
                serde_json::from_value(metadata.get("calls").cloned().unwrap_or(json!([])))?;
            node.references =
                serde_json::from_value(metadata.get("references").cloned().unwrap_or(json!([])))?;
            node.description = metadata["descriptionHash"]
                .as_str()
                .map(|key| {
                    conn.query_row(
                        "SELECT content FROM global.description_content WHERE hash=?",
                        [key],
                        |r| r.get::<_, String>(0),
                    )
                    .with_context(|| format!("Missing global description content {key}"))
                })
                .transpose()?;
            node.names = names.remove(&node.id).unwrap_or_default();
            Ok(node)
        })
        .collect::<Result<_>>()?;
    Ok(FileStructure { nodes })
}

pub(crate) fn parsed_items(file: &File, parsed: &ParsedFile) -> Result<Vec<Item>> {
    let mut occurrences = HashMap::<&str, usize>::new();
    let mut callable_symbols = HashSet::new();
    let mut items = Vec::with_capacity(parsed.callables.len() + parsed.chunks.len());
    for callable in &parsed.callables {
        let occurrence = occurrences.entry(&callable.qualified_name).or_default();
        let identity = hash(
            json!([
                file.path,
                callable.qualified_name,
                callable.kind,
                *occurrence
            ])
            .to_string(),
        );
        *occurrence += 1;
        let mut data = serde_json::to_value(callable)?;
        data["path"] = json!(file.path);
        data["sourceMode"] = json!(file.source_mode);
        data["description"] = json!(callable.description);
        data["sourceDescription"] = json!(callable.description.is_some());
        if let Some(node) = map::matching_node(&parsed.structure, &data) {
            callable_symbols.insert(node.id);
        }
        items.push(Item {
            id: 0,
            path: file.path.clone(),
            identity,
            kind: "function".into(),
            data,
            embedding: String::new(),
            description_embedding: None,
        });
    }
    // Count every noncallable declaration, including those without prose, so
    // removing a preceding comment cannot renumber a later declaration.
    occurrences.clear();
    for node in &parsed.structure.nodes {
        if callable_symbols.contains(&node.id) {
            continue;
        }
        let occurrence = occurrences.entry(&node.qualified_name).or_default();
        // Description-only declarations (including overload signatures) have
        // their own occurrence sequence, separate from callable implementations.
        let identity = hash(
            json!([
                file.path,
                "symbol-description",
                node.qualified_name,
                node.kind,
                *occurrence
            ])
            .to_string(),
        );
        *occurrence += 1;
        if node.description.is_none() {
            continue;
        }
        let mut data = serde_json::to_value(node)?;
        data["path"] = json!(file.path);
        data["sourceMode"] = json!(file.source_mode);
        data["sourceDescription"] = json!(true);
        data["sourceHash"] = json!(hash(
            file.source
                .get(node.start_byte..node.end_byte)
                .context("Invalid described symbol source range")?
        ));
        items.push(Item {
            id: 0,
            path: file.path.clone(),
            identity,
            kind: "symbol-description".into(),
            data,
            embedding: String::new(),
            description_embedding: None,
        });
    }
    for (ordinal, chunk) in parsed.chunks.iter().enumerate() {
        let kind = if crate::formats::FormatGroup::for_language(&file.language)
            == Some(crate::formats::FormatGroup::Docs)
        {
            "markdown"
        } else {
            "document"
        };
        let mut data = serde_json::to_value(chunk)?;
        data["path"] = json!(file.path);
        data["sourceMode"] = json!(file.source_mode);
        items.push(Item {
            id: 0,
            path: file.path.clone(),
            identity: hash(json!([file.path, kind, ordinal]).to_string()),
            kind: kind.into(),
            data,
            embedding: String::new(),
            description_embedding: None,
        });
    }
    Ok(items)
}

pub(super) fn symbol_for(item: &Item, structure: &FileStructure) -> Option<usize> {
    if item.kind == "document" {
        return None;
    }
    if item.kind == "symbol-description" {
        let start = item.data["startLine"].as_u64()? as usize;
        let end = item.data["endLine"].as_u64().unwrap_or(start as u64) as usize;
        return structure
            .nodes
            .iter()
            .filter(|node| {
                item.data["qualifiedName"].as_str() == Some(node.qualified_name.as_str())
                    && item.data["kind"]
                        .as_str()
                        .is_none_or(|kind| kind == node.kind)
            })
            .min_by_key(|node| {
                (
                    node.start_line.abs_diff(start) + node.end_line.abs_diff(end),
                    node.start_column
                        .abs_diff(item.data["startColumn"].as_u64().unwrap_or(1) as usize),
                    node.id,
                )
            })
            .map(|node| node.id);
    }
    if item.kind != "markdown" {
        return map::matching_node(structure, &item.data).map(|node| node.id);
    }
    let start = item.data["startLine"].as_u64()? as usize;
    let end = item.data["endLine"].as_u64().unwrap_or(start as u64) as usize;
    structure
        .nodes
        .iter()
        .filter(|node| node.kind == "heading" && node.start_line <= start && node.end_line >= start)
        .min_by_key(|node| {
            (
                node.start_line.abs_diff(start) + node.end_line.abs_diff(end),
                node.start_column
                    .abs_diff(item.data["startColumn"].as_u64().unwrap_or(1) as usize),
                node.id,
            )
        })
        .map(|node| node.id)
}

pub(super) fn associate(
    conn: &Connection,
    id: i64,
    role: &str,
    profile: Option<&Value>,
    input: &str,
    key: &str,
) -> Result<()> {
    if key.is_empty() {
        return Ok(());
    }
    validate_embedding_reference(conn, Some(key))?;
    if let Some(profile) = profile {
        ensure!(
            key == Database::embedding_key(profile, false, input),
            "Embedding reference does not match projection profile/input"
        );
    }
    let profile_key = profile.map(|p| hash(p.to_string())).unwrap_or_default();
    let input_hash = hash(input);
    // Reinsert the same association to make rowid a deterministic latest-write
    // order for items(); other profiles/inputs survive.
    conn.execute(
        "DELETE FROM unit_embeddings WHERE unit_id=? AND role=? AND profile_key=? AND input_hash=?",
        params![id, role, profile_key, input_hash],
    )?;
    conn.execute("INSERT INTO unit_embeddings(unit_id,role,profile_key,input_hash,embedding_key) VALUES(?,?,?,?,?)", params![id, role, profile_key, input_hash, key])?;
    Ok(())
}

pub(super) fn write_units(
    conn: &Connection,
    file: &File,
    items: &[Item],
    structure: &FileStructure,
    structural: bool,
    profile: Option<&Value>,
) -> Result<()> {
    let identities: HashSet<_> = items.iter().map(|i| i.identity.as_str()).collect();
    ensure!(
        identities.len() == items.len(),
        "Duplicate search unit identity for {}",
        file.path
    );
    let existing: Vec<(i64, String)> = conn
        .prepare("SELECT id,identity FROM search_units WHERE path=?")?
        .query_map([&file.path], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, identity) in existing {
        if !identities.contains(identity.as_str()) {
            conn.execute("DELETE FROM search_units WHERE id=?", [id])?;
            conn.execute(
                "DELETE FROM descriptions WHERE scope='callable' AND path=? AND identity=?",
                params![file.path, identity],
            )?;
        }
    }
    for item in items {
        ensure!(
            item.path == file.path && item.data.is_object(),
            "Invalid search unit for {}",
            file.path
        );
        let previous: Option<(String, String)> = conn
            .query_row(
                "SELECT path,data FROM search_units WHERE identity=?",
                [&item.identity],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        ensure!(
            previous.as_ref().is_none_or(|(p, _)| p == &file.path),
            "Search unit identity belongs to another file"
        );
        let old_source_description = previous
            .map(|(_, data)| serde_json::from_str::<Value>(&data))
            .transpose()?
            .is_some_and(|data| data["sourceDescription"] == true);
        let mut data = item.data.clone();
        let old_description = description(conn, "callable", &file.path, &item.identity)?;
        let symbol = symbol_for(item, structure);
        // Structural publication is authoritative for comment removal. During
        // semantic preparation, canonical source prose outranks generated prose.
        if !structural
            && matches!(item.kind.as_str(), "function" | "symbol-description")
            && let Some(text) = symbol
                .and_then(|id| structure.nodes.iter().find(|node| node.id == id))
                .and_then(|node| node.description.as_ref())
        {
            data["description"] = json!(text);
            data["sourceDescription"] = json!(true);
        }
        if structural
            && data["sourceDescription"] != true
            && !old_source_description
            && let Some(d) = &old_description
            && d.0.as_deref() == data["sourceHash"].as_str()
        {
            data["description"] = json!(d.1);
        }
        ensure!(
            data["sourceDescription"] != true || data["description"].is_string(),
            "Source description has no description text"
        );
        let input = data["embeddingInput"].as_str().unwrap_or("");
        let input_hash = hash(input);
        let mut stored_data = data.clone();
        let payload = unit_payload(&data)?;
        for field in ["source", "embeddingInput"] {
            stored_data.as_object_mut().unwrap().remove(field);
        }
        let content_hash = hash(&payload);
        ensure!(conn.query_row("SELECT EXISTS(SELECT 1 FROM global.cache WHERE kind='unit-content' AND key=? AND value=?)",
            params![content_hash, payload], |row| row.get::<_, bool>(0))?, "Missing or corrupt global unit content artifact {content_hash}");
        if stored_data.get("description").is_some() {
            stored_data["description"] = Value::Null;
        }
        conn.execute("INSERT INTO search_units(path,identity,kind,symbol_id,data,embedding_input_hash,content_hash) VALUES(?,?,?,?,?,?,?) ON CONFLICT(identity) DO UPDATE SET
            kind=excluded.kind,symbol_id=excluded.symbol_id,data=excluded.data,embedding_input_hash=excluded.embedding_input_hash,content_hash=excluded.content_hash",
            params![item.path, item.identity, item.kind, symbol.map(i64::try_from).transpose()?, stored_data.to_string(), input_hash, content_hash])?;
        let id: i64 = conn.query_row(
            "SELECT id FROM search_units WHERE identity=?",
            [&item.identity],
            |r| r.get(0),
        )?;
        if item.kind != "symbol-description" {
            associate(conn, id, "code", profile, input, &item.embedding)?;
        }
        conn.execute(
            "DELETE FROM descriptions WHERE scope='callable' AND path=? AND identity=?",
            params![item.path, item.identity],
        )?;
        if let Some(text) = data["description"].as_str() {
            let embedding = if !structural && data["description"] == item.data["description"] {
                item.description_embedding.as_deref()
            } else if structural {
                old_description
                    .as_ref()
                    .filter(|d| d.1 == text)
                    .and_then(|d| d.2.as_deref())
            } else {
                None
            };
            let content_hash = description_reference(conn, text)?;
            validate_embedding_reference(conn, embedding)?;
            conn.execute("INSERT INTO descriptions(scope,path,identity,source_hash,content_hash,embedding_key) VALUES('callable',?,?,?,?,?)",
                params![item.path, item.identity, data["sourceHash"].as_str(), content_hash, embedding])?;
            if !structural && let Some(key) = embedding {
                associate(conn, id, "description", profile, text, key)?;
            }
        } else {
            ensure!(
                item.description_embedding
                    .as_deref()
                    .is_none_or(str::is_empty),
                "Description vector has no description text"
            );
        }
    }
    Ok(())
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<f32>> {
    ensure!(
        !bytes.is_empty() && bytes.len().is_multiple_of(4),
        "Invalid stored vector length"
    );
    let values: Vec<_> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    ensure!(
        values.iter().all(|v| v.is_finite()),
        "Non-finite stored vector"
    );
    Ok(values)
}
