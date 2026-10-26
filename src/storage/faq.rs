//! Workspace-local FAQ membership; provider vectors remain in the artifact store.
use super::*;
use std::collections::BTreeMap;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS faq_entries(
 id INTEGER PRIMARY KEY AUTOINCREMENT,contract TEXT NOT NULL,question TEXT NOT NULL,
 result TEXT NOT NULL,files TEXT NOT NULL,question_embedding TEXT NOT NULL,answer_embedding TEXT NOT NULL,
 UNIQUE(contract,question));
";

pub(crate) struct FaqEntry {
    pub id: u64,
    pub question: String,
    pub result: Value,
    pub files: BTreeMap<String, String>,
    pub question_embedding: String,
    pub answer_embedding: String,
}

pub(crate) fn exists(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='faq_entries')",
        [],
        |row| row.get(0),
    )?)
}

impl Database {
    pub(crate) fn faq_entries(&self, contract: &str) -> Result<Vec<FaqEntry>> {
        Ok(self
            .read_faq(Some(contract))?
            .into_iter()
            .map(|(_, entry)| entry)
            .collect())
    }

    pub(crate) fn all_faq_entries(&self) -> Result<Vec<(String, FaqEntry)>> {
        self.read_faq(None)
    }

    fn read_faq(&self, contract: Option<&str>) -> Result<Vec<(String, FaqEntry)>> {
        if !exists(&self.conn)? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare("SELECT id,question,result,files,question_embedding,answer_embedding,contract FROM faq_entries WHERE (?1 IS NULL OR contract=?1) ORDER BY id")?;
        let rows = stmt.query_map([contract], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        rows.map(|row| {
            let (id, question, result, files, question_embedding, answer_embedding, contract) =
                row?;
            Ok((
                contract,
                FaqEntry {
                    id: u64::try_from(id).context("Invalid FAQ id")?,
                    question,
                    result: serde_json::from_str(&result).context("Invalid FAQ result")?,
                    files: serde_json::from_str(&files).context("Invalid FAQ file dependencies")?,
                    question_embedding,
                    answer_embedding,
                },
            ))
        })
        .collect()
    }

    pub(crate) fn put_faq(&self, contract: &str, entry: &FaqEntry) -> Result<()> {
        validate_embedding_reference(&self.conn, Some(&entry.question_embedding))?;
        validate_embedding_reference(&self.conn, Some(&entry.answer_embedding))?;
        let tx = self.conn.unchecked_transaction()?;
        // Additive, on-demand migration: ordinary warm opens never acquire a writer.
        if !exists(&tx)? {
            tx.execute_batch(SCHEMA)?;
        }
        tx.execute("INSERT INTO faq_entries(contract,question,result,files,question_embedding,answer_embedding) VALUES(?,?,?,?,?,?) ON CONFLICT(contract,question) DO UPDATE SET result=excluded.result,files=excluded.files,question_embedding=excluded.question_embedding,answer_embedding=excluded.answer_embedding", params![contract,entry.question,entry.result.to_string(),serde_json::to_string(&entry.files)?,entry.question_embedding,entry.answer_embedding])?;
        tx.commit()?;
        Ok(())
    }
}
