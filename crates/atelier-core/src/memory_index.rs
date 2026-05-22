//! Derived SQLite index for durable memory cards.
//!
//! Canonical memory stays as human-editable Markdown under
//! `<repo>/.atelier/memory/` or `~/.atelier/memory/`. This module builds a
//! rebuildable SQLite/FTS5 index under `.atelier/indexes/memory.sqlite` so
//! future chat/git-history memory extraction and recall can search cards
//! without treating the database as source-of-truth.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{params, Connection};

pub const ATELIER_DIR: &str = ".atelier";
pub const MEMORY_DIR: &str = "memory";
pub const INDEXES_DIR: &str = "indexes";
pub const MEMORY_INDEX_FILE: &str = "memory.sqlite";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    User,
    Project,
}

impl MemoryScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedMemoryCard {
    pub path: PathBuf,
    pub scope: MemoryScope,
    pub id: String,
    pub title: String,
    pub description: String,
    pub body: String,
    pub tags: Vec<String>,
    pub mtime_unix_nanos: i64,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySearchHit {
    pub path: PathBuf,
    pub scope: String,
    pub id: String,
    pub title: String,
    pub description: String,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryIndexStats {
    pub indexed_cards: usize,
    pub skipped_files: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryIndexError {
    #[error("memory index I/O error at {path:?}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("memory index SQLite error at {path:?}: {source}")]
    Sqlite {
        path: PathBuf,
        source: rusqlite::Error,
    },
}

impl PartialEq for MemoryIndexError {
    fn eq(&self, other: &Self) -> bool {
        self.to_string() == other.to_string()
    }
}

impl Eq for MemoryIndexError {}

pub fn project_memory_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(ATELIER_DIR).join(MEMORY_DIR)
}

pub fn project_memory_index_path(repo_root: &Path) -> PathBuf {
    repo_root
        .join(ATELIER_DIR)
        .join(INDEXES_DIR)
        .join(MEMORY_INDEX_FILE)
}

pub fn user_memory_dir(home: &Path) -> PathBuf {
    home.join(ATELIER_DIR).join(MEMORY_DIR)
}

pub fn user_memory_index_path(home: &Path) -> PathBuf {
    home.join(ATELIER_DIR)
        .join(INDEXES_DIR)
        .join(MEMORY_INDEX_FILE)
}

pub fn rebuild_project_memory_index(
    repo_root: &Path,
) -> Result<MemoryIndexStats, MemoryIndexError> {
    rebuild_memory_index(
        &project_memory_dir(repo_root),
        &project_memory_index_path(repo_root),
        MemoryScope::Project,
    )
}

pub fn rebuild_user_memory_index(home: &Path) -> Result<MemoryIndexStats, MemoryIndexError> {
    rebuild_memory_index(
        &user_memory_dir(home),
        &user_memory_index_path(home),
        MemoryScope::User,
    )
}

pub fn rebuild_memory_index(
    memory_dir: &Path,
    index_path: &Path,
    scope: MemoryScope,
) -> Result<MemoryIndexStats, MemoryIndexError> {
    let conn = open_index(index_path)?;
    conn.execute("DELETE FROM memory_cards_fts", [])
        .map_err(|source| sqlite_err(index_path, source))?;
    conn.execute("DELETE FROM memory_cards", [])
        .map_err(|source| sqlite_err(index_path, source))?;

    let read_dir = match fs::read_dir(memory_dir) {
        Ok(read_dir) => read_dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MemoryIndexStats {
                indexed_cards: 0,
                skipped_files: 0,
            });
        }
        Err(source) => {
            return Err(MemoryIndexError::Io {
                path: memory_dir.to_path_buf(),
                source,
            })
        }
    };

    let mut indexed_cards = 0usize;
    let mut skipped_files = 0usize;
    for entry in read_dir {
        let entry = entry.map_err(|source| MemoryIndexError::Io {
            path: memory_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if !is_memory_card_path(&path) {
            skipped_files += 1;
            continue;
        }
        match indexable_card_from_path(&path, scope) {
            Ok(card) => {
                insert_card(&conn, index_path, &card)?;
                indexed_cards += 1;
            }
            Err(MemoryIndexError::Io { .. }) => {
                skipped_files += 1;
            }
            Err(e) => return Err(e),
        }
    }

    Ok(MemoryIndexStats {
        indexed_cards,
        skipped_files,
    })
}

pub fn upsert_memory_card_file(
    card_path: &Path,
    index_path: &Path,
    scope: MemoryScope,
) -> Result<(), MemoryIndexError> {
    let conn = open_index(index_path)?;
    let key = card_path.to_string_lossy().to_string();
    conn.execute("DELETE FROM memory_cards_fts WHERE path = ?1", [&key])
        .map_err(|source| sqlite_err(index_path, source))?;
    conn.execute("DELETE FROM memory_cards WHERE path = ?1", [&key])
        .map_err(|source| sqlite_err(index_path, source))?;
    if is_memory_card_path(card_path) {
        let card = indexable_card_from_path(card_path, scope)?;
        insert_card(&conn, index_path, &card)?;
    }
    Ok(())
}

pub fn remove_memory_card_file(
    card_path: &Path,
    index_path: &Path,
) -> Result<(), MemoryIndexError> {
    let conn = open_index(index_path)?;
    let key = card_path.to_string_lossy().to_string();
    conn.execute("DELETE FROM memory_cards_fts WHERE path = ?1", [&key])
        .map_err(|source| sqlite_err(index_path, source))?;
    conn.execute("DELETE FROM memory_cards WHERE path = ?1", [&key])
        .map_err(|source| sqlite_err(index_path, source))?;
    Ok(())
}

pub fn search_memory_index(
    index_path: &Path,
    query: &str,
    limit: usize,
) -> Result<Vec<MemorySearchHit>, MemoryIndexError> {
    if query.trim().is_empty() || limit == 0 || !index_path.exists() {
        return Ok(Vec::new());
    }
    let conn = open_index(index_path)?;
    let fts_query = fts_query(query);
    let mut stmt = conn
        .prepare(
            "SELECT c.path, c.scope, c.id, c.title, c.description,
                    snippet(memory_cards_fts, 5, '', '', '...', 24) AS snippet
             FROM memory_cards_fts
             JOIN memory_cards c ON c.path = memory_cards_fts.path
             WHERE memory_cards_fts MATCH ?1
             ORDER BY bm25(memory_cards_fts)
             LIMIT ?2",
        )
        .map_err(|source| sqlite_err(index_path, source))?;
    let rows = stmt
        .query_map(params![fts_query, limit as i64], |row| {
            Ok(MemorySearchHit {
                path: PathBuf::from(row.get::<_, String>(0)?),
                scope: row.get(1)?,
                id: row.get(2)?,
                title: row.get(3)?,
                description: row.get(4)?,
                snippet: row.get(5)?,
            })
        })
        .map_err(|source| sqlite_err(index_path, source))?;
    let mut hits = Vec::new();
    for row in rows {
        hits.push(row.map_err(|source| sqlite_err(index_path, source))?);
    }
    Ok(hits)
}

fn open_index(index_path: &Path) -> Result<Connection, MemoryIndexError> {
    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent).map_err(|source| MemoryIndexError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let conn = Connection::open(index_path).map_err(|source| sqlite_err(index_path, source))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS memory_cards (
             path TEXT PRIMARY KEY,
             scope TEXT NOT NULL,
             id TEXT NOT NULL,
             title TEXT NOT NULL,
             description TEXT NOT NULL,
             body TEXT NOT NULL,
             tags TEXT NOT NULL,
             mtime_unix_nanos INTEGER NOT NULL,
             size_bytes INTEGER NOT NULL
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS memory_cards_fts
         USING fts5(
             path UNINDEXED,
             scope UNINDEXED,
             id,
             title,
             description,
             body,
             tags
         );",
    )
    .map_err(|source| sqlite_err(index_path, source))?;
    Ok(conn)
}

fn insert_card(
    conn: &Connection,
    index_path: &Path,
    card: &IndexedMemoryCard,
) -> Result<(), MemoryIndexError> {
    let path = card.path.to_string_lossy().to_string();
    let scope = card.scope.as_str();
    let tags = card.tags.join(",");
    conn.execute(
        "INSERT OR REPLACE INTO memory_cards
         (path, scope, id, title, description, body, tags, mtime_unix_nanos, size_bytes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            path,
            scope,
            card.id,
            card.title,
            card.description,
            card.body,
            tags,
            card.mtime_unix_nanos,
            card.size_bytes as i64,
        ],
    )
    .map_err(|source| sqlite_err(index_path, source))?;
    conn.execute(
        "INSERT INTO memory_cards_fts (path, scope, id, title, description, body, tags)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            path,
            scope,
            card.id,
            card.title,
            card.description,
            card.body,
            tags,
        ],
    )
    .map_err(|source| sqlite_err(index_path, source))?;
    Ok(())
}

fn indexable_card_from_path(
    path: &Path,
    scope: MemoryScope,
) -> Result<IndexedMemoryCard, MemoryIndexError> {
    let raw = fs::read_to_string(path).map_err(|source| MemoryIndexError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = fs::metadata(path).map_err(|source| MemoryIndexError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mtime_unix_nanos = metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    let (frontmatter, body) = split_frontmatter(&raw);
    let attrs = parse_frontmatter(frontmatter.unwrap_or(""));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("memory")
        .to_string();
    let body = body.trim().to_string();
    let id = first_attr(&attrs, &["id", "name"]).unwrap_or(stem.clone());
    let title = first_attr(&attrs, &["title"])
        .or_else(|| first_non_empty_line(&body).map(str::to_string))
        .unwrap_or_else(|| id.clone());
    let description = first_attr(&attrs, &["description"]).unwrap_or_default();
    let tags = parse_tags(&attrs);
    Ok(IndexedMemoryCard {
        path: path.to_path_buf(),
        scope,
        id,
        title,
        description,
        body,
        tags,
        mtime_unix_nanos,
        size_bytes: metadata.len(),
    })
}

fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let Some(after_open) = raw.strip_prefix("---") else {
        return (None, raw);
    };
    let Some(after_open) = after_open.strip_prefix('\n') else {
        return (None, raw);
    };
    if let Some(idx) = after_open.find("\n---\n") {
        let frontmatter = &after_open[..idx];
        let body = &after_open[idx + "\n---\n".len()..];
        return (Some(frontmatter), body);
    }
    if let Some(frontmatter) = after_open.strip_suffix("\n---") {
        return (Some(frontmatter), "");
    }
    (None, raw)
}

fn parse_frontmatter(frontmatter: &str) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current_list_key: Option<String> = None;
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("- ") {
            if let Some(key) = current_list_key.as_ref() {
                out.entry(key.clone())
                    .or_default()
                    .push(unquote_yaml_scalar(rest));
            }
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            current_list_key = None;
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if value.is_empty() {
            current_list_key = Some(key.clone());
            out.entry(key).or_default();
        } else {
            current_list_key = None;
            out.entry(key).or_default().push(unquote_yaml_scalar(value));
        }
    }
    out
}

fn first_attr(attrs: &BTreeMap<String, Vec<String>>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| attrs.get(*key))
        .flat_map(|values| values.iter())
        .find(|value| !value.trim().is_empty())
        .cloned()
}

fn parse_tags(attrs: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let Some(values) = attrs.get("tags") else {
        return Vec::new();
    };
    values
        .iter()
        .flat_map(|value| value.split(','))
        .map(|value| value.trim().trim_matches(['[', ']']).to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn unquote_yaml_scalar(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn first_non_empty_line(body: &str) -> Option<&str> {
    body.lines().map(str::trim).find(|line| !line.is_empty())
}

fn is_memory_card_path(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()) == Some("md")
        && path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|name| name != "MEMORY.md" && name != "README.md" && !name.starts_with('.'))
            .unwrap_or(false)
}

fn fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn sqlite_err(path: &Path, source: rusqlite::Error) -> MemoryIndexError {
    MemoryIndexError::Sqlite {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn rebuild_indexes_markdown_cards_and_skips_docs() {
        let tmp = tempfile::tempdir().unwrap();
        let memory_dir = tmp.path().join(".atelier/memory");
        let index_path = tmp.path().join(".atelier/indexes/memory.sqlite");
        write(
            &memory_dir.join("provider_routing.md"),
            "---\nname: provider-routing\ndescription: Routing note\ntags:\n  - providers\n---\n\nPlanner stays on the primary adapter.\n",
        );
        write(&memory_dir.join("README.md"), "# docs only");

        let stats = rebuild_memory_index(&memory_dir, &index_path, MemoryScope::Project).unwrap();
        assert_eq!(stats.indexed_cards, 1);
        assert_eq!(stats.skipped_files, 1);

        let hits = search_memory_index(&index_path, "primary", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "provider-routing");
        assert_eq!(hits[0].description, "Routing note");
    }

    #[test]
    fn upsert_replaces_existing_index_row() {
        let tmp = tempfile::tempdir().unwrap();
        let memory_dir = tmp.path().join(".atelier/memory");
        let index_path = tmp.path().join(".atelier/indexes/memory.sqlite");
        let card = memory_dir.join("fact.md");
        write(&card, "---\nname: fact\n---\n\nold body");
        upsert_memory_card_file(&card, &index_path, MemoryScope::Project).unwrap();
        write(&card, "---\nname: fact\n---\n\nnew searchable body");
        upsert_memory_card_file(&card, &index_path, MemoryScope::Project).unwrap();

        assert!(search_memory_index(&index_path, "old", 10)
            .unwrap()
            .is_empty());
        let hits = search_memory_index(&index_path, "searchable", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "new searchable body");
    }

    #[test]
    fn path_helpers_keep_indexes_outside_memory_dir() {
        let root = Path::new("/repo");
        assert_eq!(
            project_memory_dir(root),
            PathBuf::from("/repo/.atelier/memory")
        );
        assert_eq!(
            project_memory_index_path(root),
            PathBuf::from("/repo/.atelier/indexes/memory.sqlite")
        );
    }
}
