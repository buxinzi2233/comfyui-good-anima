use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OpenFlags, params_from_iter};

#[derive(Debug, Clone)]
pub struct DbRow {
    pub tag: String,
    pub count: i64,
    pub aliases: String,
}

/// Environment variable that overrides index discovery with an explicit path.
pub const DB_ENV_VAR: &str = "DANBOORU_TAGS_DB";

const DB_FILE: &str = "tags_index.sqlite";
const EXTENDED_ARTISTS_FILE: &str = "artists_extended.txt";
const PARENT_LEVELS: usize = 3;

/// Resolve the tag index path.
///
/// Lookup order:
/// 1. `$DANBOORU_TAGS_DB` (must exist; never falls through silently)
/// 2. directory of the running executable and up to 3 of its parents (symlinks resolved)
/// 3. directory of the invocation path (`argv[0]`, PATH-resolved) and its parents
/// 4. current working directory
///
/// No build-time path is embedded, so a relocated binary never silently reads a
/// stale index location.
pub fn db_path() -> Result<PathBuf> {
    if let Some(raw) = env::var_os(DB_ENV_VAR) {
        let value = raw.to_string_lossy().trim().to_string();
        if value.is_empty() {
            return Err(anyhow!("{DB_ENV_VAR} is set but empty"));
        }
        let path = PathBuf::from(&value);
        if path.is_file() {
            return Ok(path);
        }
        return Err(anyhow!(
            "{DB_ENV_VAR} points to a missing file: {}",
            path.display()
        ));
    }

    let dirs = candidate_dirs();
    for dir in &dirs {
        let candidate = dir.join(DB_FILE);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(anyhow!(
        "{DB_FILE} not found. searched: {}. set {DB_ENV_VAR}=/abs/path/{DB_FILE} or keep the index next to the executable",
        dirs.iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// Directories searched for index assets, in priority order and de-duplicated.
pub fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = env::current_exe() {
        push_dir_with_parents(&mut dirs, &exe, PARENT_LEVELS);
    }

    if let Some(argv0) = env::args_os().next() {
        let raw = PathBuf::from(argv0);
        let resolved = if raw.is_absolute() || raw.components().count() > 1 {
            Some(raw)
        } else {
            env::var_os("PATH").and_then(|value| {
                env::split_paths(&value)
                    .map(|dir| dir.join(&raw))
                    .find(|candidate| candidate.is_file())
            })
        };
        if let Some(path) = resolved {
            push_dir_with_parents(&mut dirs, &path, PARENT_LEVELS);
        }
    }

    if let Ok(cwd) = env::current_dir() {
        push_dir(&mut dirs, cwd);
    }

    dirs
}

fn push_dir_with_parents(dirs: &mut Vec<PathBuf>, file_path: &Path, levels: usize) {
    let mut current = file_path.parent().map(Path::to_path_buf);
    let mut remaining = levels;
    while let Some(dir) = current {
        push_dir(dirs, dir.clone());
        if remaining == 0 {
            break;
        }
        remaining -= 1;
        current = dir.parent().map(Path::to_path_buf);
    }
}

fn push_dir(dirs: &mut Vec<PathBuf>, dir: PathBuf) {
    if dir.as_os_str().is_empty() {
        return;
    }
    if !dirs.iter().any(|existing| existing == &dir) {
        dirs.push(dir);
    }
}

/// Index statistics used by `--check-index` as a liveness probe.
///
/// Fails when the index cannot be resolved or opened, so callers never mistake a
/// missing index for "no matching tag".
pub fn index_stats() -> Result<(PathBuf, i64, i64)> {
    let path = db_path()?;
    let conn = open_connection()?;
    let tags: i64 = conn.query_row("SELECT COUNT(*) FROM tags", [], |row| row.get(0))?;
    let groups: i64 = conn.query_row("SELECT COUNT(*) FROM tag_groups", [], |row| row.get(0))?;
    Ok((path, tags, groups))
}

pub fn load_extended_artists() -> Result<Vec<String>> {
    let Some(path) = find_asset(EXTENDED_ARTISTS_FILE) else {
        eprintln!("warning: {EXTENDED_ARTISTS_FILE} not found in the searched directories; --extended has no extra artists to draw from");
        return Ok(Vec::new());
    };
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    Ok(data
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

pub fn query_artist_tags() -> Result<Vec<String>> {
    let conn = open_connection()?;
    let mut stmt = conn.prepare(
        "SELECT tag FROM tags WHERE category = 'artists' AND tag LIKE '@%' ORDER BY count DESC, tag ASC",
    )?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn query_random_pool(category: &str, group_name: &str, min_count: i64) -> Result<Vec<DbRow>> {
    if category.is_empty() {
        return Ok(Vec::new());
    }

    let mut sql = String::from("SELECT t.tag, t.count, t.aliases FROM tags t");
    let mut clauses = vec!["t.category = ?".to_string()];
    let mut params: Vec<String> = vec![category.to_string()];

    if should_join_group(group_name) {
        sql.push_str(" JOIN tag_groups g ON g.category = t.category AND g.tag = t.tag");
        clauses.push("g.group_name = ?".to_string());
        params.push(group_name.to_string());
    }
    if min_count > 0 {
        clauses.push("t.count >= ?".to_string());
        params.push(min_count.to_string());
    }

    let sql = format!(
        "{sql} WHERE {} ORDER BY t.count DESC, t.tag ASC",
        clauses.join(" AND ")
    );

    let conn = open_connection()?;
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(params.iter()), |row| {
            Ok(DbRow {
                tag: row.get(0)?,
                count: row.get(1)?,
                aliases: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn query_rows(
    category: &str,
    group_name: &str,
    min_count: i64,
    prefix_norm: &str,
    keyword_terms: &[String],
    limit: usize,
) -> Result<Vec<DbRow>> {
    if category.is_empty() {
        return Ok(Vec::new());
    }

    let mut sql = String::from("SELECT t.tag, t.count, t.aliases FROM tags t");
    let mut clauses = vec!["t.category = ?".to_string()];
    let mut params: Vec<String> = vec![category.to_string()];

    if should_join_group(group_name) {
        sql.push_str(" JOIN tag_groups g ON g.category = t.category AND g.tag = t.tag");
        clauses.push("g.group_name = ?".to_string());
        params.push(group_name.to_string());
    }
    if min_count > 0 {
        clauses.push("t.count >= ?".to_string());
        params.push(min_count.to_string());
    }
    if !prefix_norm.is_empty() {
        clauses.push("t.tag_norm LIKE ?".to_string());
        params.push(format!("{prefix_norm}%"));
    }
    if !keyword_terms.is_empty() {
        let mut term_clauses = Vec::new();
        for term in keyword_terms {
            term_clauses.push("(t.tag_norm LIKE ? OR t.aliases_norm LIKE ?)".to_string());
            let like = format!("%{}%", normalize_for_sql(term));
            params.push(like.clone());
            params.push(like);
        }
        clauses.push(format!("({})", term_clauses.join(" OR ")));
    }

    let candidate_limit = std::cmp::max(limit.saturating_mul(20), 200);
    let sql = format!(
        "{sql} WHERE {} ORDER BY t.count DESC, t.tag ASC LIMIT ?",
        clauses.join(" AND ")
    );
    params.push(candidate_limit.to_string());

    let conn = open_connection()?;
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(params.iter()), |row| {
            Ok(DbRow {
                tag: row.get(0)?,
                count: row.get(1)?,
                aliases: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn query_exact_rows(
    category: &str,
    group_name: &str,
    min_count: i64,
    prefix_norm: &str,
    primary_norm: &str,
    limit: usize,
) -> Result<Vec<DbRow>> {
    if category.is_empty() {
        return Ok(Vec::new());
    }

    let mut sql = String::from("SELECT t.tag, t.count, t.aliases FROM tags t");
    let mut clauses = vec!["t.category = ?".to_string()];
    let mut params: Vec<String> = vec![category.to_string()];

    if should_join_group(group_name) {
        sql.push_str(" JOIN tag_groups g ON g.category = t.category AND g.tag = t.tag");
        clauses.push("g.group_name = ?".to_string());
        params.push(group_name.to_string());
    }
    if min_count > 0 {
        clauses.push("t.count >= ?".to_string());
        params.push(min_count.to_string());
    }
    if !prefix_norm.is_empty() {
        clauses.push("t.tag_norm LIKE ?".to_string());
        params.push(format!("{prefix_norm}%"));
    } else if !primary_norm.is_empty() {
        clauses.push("(t.tag_norm = ? OR t.aliases_norm LIKE ?)".to_string());
        params.push(normalize_for_sql(primary_norm));
        params.push(format!("%{}%", normalize_for_sql(primary_norm)));
    } else {
        return Ok(Vec::new());
    }

    let sql = format!(
        "{sql} WHERE {} ORDER BY t.count DESC, t.tag ASC LIMIT ?",
        clauses.join(" AND ")
    );
    params.push(std::cmp::max(limit.saturating_mul(4), 20).to_string());

    let conn = open_connection()?;
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(params.iter()), |row| {
            Ok(DbRow {
                tag: row.get(0)?,
                count: row.get(1)?,
                aliases: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn open_connection() -> Result<Connection> {
    let path = db_path()?;
    Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open {}", path.display()))
}

pub fn find_asset(file_name: &str) -> Option<PathBuf> {
    candidate_dirs()
        .into_iter()
        .map(|dir| dir.join(file_name))
        .find(|candidate| candidate.is_file())
}

fn should_join_group(group_name: &str) -> bool {
    !group_name.is_empty() && group_name != "characters" && group_name != "series"
}

fn normalize_for_sql(value: &str) -> String {
    let mut s = value.trim().to_lowercase();
    if s.starts_with('@') {
        s = s[1..].trim().to_string();
    }
    s = s.replace(r"\(", "(").replace(r"\)", ")");
    s = s.replace('-', " ").replace('_', " ");
    collapse_ws(&s)
}

fn collapse_ws(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
