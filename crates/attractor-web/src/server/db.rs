use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::path::PathBuf;

/// Project stored in the database
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Project {
    pub id: i64,
    pub folder_path: String,
    pub name: String,
}

/// Cached document content
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CachedDoc {
    pub doc_type: String,
    pub content: String,
}

/// Initialize the SQLite database and return a connection pool.
///
/// Creates `~/.pas/web.db` if it doesn't exist, along with the directory.
/// Runs schema creation (idempotent).
pub async fn init_db() -> Result<SqlitePool, sqlx::Error> {
    // Determine database path: ~/.pas/web.db
    let home_dir = std::env::var("HOME")
        .expect("HOME environment variable not set");
    let pas_dir = PathBuf::from(home_dir).join(".pas");

    // Create directory if it doesn't exist
    std::fs::create_dir_all(&pas_dir)
        .expect("Failed to create ~/.pas directory");

    let db_path = pas_dir.join("web.db");
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());

    // Create connection pool
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await?;

    // Create tables (idempotent)
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS projects (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            folder_path TEXT    NOT NULL UNIQUE,
            name        TEXT    NOT NULL,
            is_open     INTEGER NOT NULL DEFAULT 1,
            last_used   TEXT    NOT NULL DEFAULT (datetime('now')),
            created_at  TEXT    NOT NULL DEFAULT (datetime('now'))
        )
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS documents (
            project_id  INTEGER NOT NULL REFERENCES projects(id),
            doc_type    TEXT    NOT NULL,
            content     TEXT    NOT NULL DEFAULT '',
            updated_at  TEXT    NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (project_id, doc_type)
        )
        "#,
    )
    .execute(&pool)
    .await?;

    Ok(pool)
}

/// Insert or reopen a project. Returns the project record.
///
/// If the project doesn't exist, it's created with the folder basename as the name.
/// If it exists, `is_open` is set to 1 and `last_used` is updated.
pub async fn upsert_project(
    pool: &SqlitePool,
    folder_path: &str,
) -> Result<Project, sqlx::Error> {
    // Extract project name from folder path
    let name = PathBuf::from(folder_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    // Insert if not exists
    sqlx::query(
        r#"
        INSERT OR IGNORE INTO projects (folder_path, name)
        VALUES (?, ?)
        "#,
    )
    .bind(folder_path)
    .bind(&name)
    .execute(pool)
    .await?;

    // Update last_used and set is_open=1
    sqlx::query(
        r#"
        UPDATE projects
        SET is_open = 1, last_used = datetime('now')
        WHERE folder_path = ?
        "#,
    )
    .bind(folder_path)
    .execute(pool)
    .await?;

    // Fetch and return the project
    let project = sqlx::query_as::<_, (i64, String, String)>(
        r#"
        SELECT id, folder_path, name
        FROM projects
        WHERE folder_path = ?
        "#,
    )
    .bind(folder_path)
    .fetch_one(pool)
    .await?;

    Ok(Project {
        id: project.0,
        folder_path: project.1,
        name: project.2,
    })
}

/// List all open projects, ordered by most recently used first.
pub async fn list_open_projects(pool: &SqlitePool) -> Result<Vec<Project>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (i64, String, String)>(
        r#"
        SELECT id, folder_path, name
        FROM projects
        WHERE is_open = 1
        ORDER BY last_used DESC
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, folder_path, name)| Project {
            id,
            folder_path,
            name,
        })
        .collect())
}

/// Get a single project by ID.
pub async fn get_project(pool: &SqlitePool, project_id: i64) -> Result<Project, sqlx::Error> {
    let row = sqlx::query_as::<_, (i64, String, String)>(
        r#"
        SELECT id, folder_path, name
        FROM projects
        WHERE id = ?
        "#,
    )
    .bind(project_id)
    .fetch_one(pool)
    .await?;

    Ok(Project {
        id: row.0,
        folder_path: row.1,
        name: row.2,
    })
}

/// Close a project (set is_open=0). Does not delete.
pub async fn close_project(pool: &SqlitePool, project_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE projects
        SET is_open = 0
        WHERE id = ?
        "#,
    )
    .bind(project_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Store or update document content for a project.
pub async fn upsert_document(
    pool: &SqlitePool,
    project_id: i64,
    doc_type: &str,
    content: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT OR REPLACE INTO documents (project_id, doc_type, content, updated_at)
        VALUES (?, ?, ?, datetime('now'))
        "#,
    )
    .bind(project_id)
    .bind(doc_type)
    .bind(content)
    .execute(pool)
    .await?;

    Ok(())
}

/// Get all cached documents for a project.
pub async fn get_documents(
    pool: &SqlitePool,
    project_id: i64,
) -> Result<Vec<CachedDoc>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT doc_type, content
        FROM documents
        WHERE project_id = ?
        "#,
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(doc_type, content)| CachedDoc { doc_type, content })
        .collect())
}

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;
