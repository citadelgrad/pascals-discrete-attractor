use super::*;

/// Helper to create a temporary database for testing
async fn temp_db() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect("sqlite::memory:")
        .await
        .expect("Failed to create in-memory database");

    // Create tables
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
    .await
    .expect("Failed to create projects table");

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
    .await
    .expect("Failed to create documents table");

    pool
}

#[tokio::test]
async fn upsert_project_creates_new_project() {
    let pool = temp_db().await;

    let project = upsert_project(&pool, "/home/user/my-project")
        .await
        .expect("Failed to upsert project");

    assert_eq!(project.folder_path, "/home/user/my-project");
    assert_eq!(project.name, "my-project");
    assert!(project.id > 0);
}

#[tokio::test]
async fn upsert_project_reopens_closed_project() {
    let pool = temp_db().await;

    // Create project
    let project1 = upsert_project(&pool, "/home/user/my-project")
        .await
        .expect("Failed to create project");

    // Close it
    close_project(&pool, project1.id)
        .await
        .expect("Failed to close project");

    // Verify it's closed
    let open_projects = list_open_projects(&pool)
        .await
        .expect("Failed to list projects");
    assert_eq!(open_projects.len(), 0);

    // Reopen by upserting again
    let project2 = upsert_project(&pool, "/home/user/my-project")
        .await
        .expect("Failed to reopen project");

    assert_eq!(project1.id, project2.id);
    assert_eq!(project2.folder_path, "/home/user/my-project");

    // Verify it's open now
    let open_projects = list_open_projects(&pool)
        .await
        .expect("Failed to list projects");
    assert_eq!(open_projects.len(), 1);
}

#[tokio::test]
async fn list_open_projects_returns_only_open() {
    let pool = temp_db().await;

    // Create three projects
    let p1 = upsert_project(&pool, "/home/user/project-a")
        .await
        .expect("Failed to create project-a");
    let _p2 = upsert_project(&pool, "/home/user/project-b")
        .await
        .expect("Failed to create project-b");
    let _p3 = upsert_project(&pool, "/home/user/project-c")
        .await
        .expect("Failed to create project-c");

    // Close one
    close_project(&pool, p1.id)
        .await
        .expect("Failed to close project");

    // Should only return 2 open projects
    let open_projects = list_open_projects(&pool)
        .await
        .expect("Failed to list projects");
    assert_eq!(open_projects.len(), 2);

    let names: Vec<String> = open_projects.iter().map(|p| p.name.clone()).collect();
    assert!(names.contains(&"project-b".to_string()));
    assert!(names.contains(&"project-c".to_string()));
    assert!(!names.contains(&"project-a".to_string()));
}

#[tokio::test]
async fn list_open_projects_ordered_by_most_recent() {
    let pool = temp_db().await;

    // Create projects in order
    let _p1 = upsert_project(&pool, "/home/user/old-project")
        .await
        .expect("Failed to create old-project");

    // Longer delay to ensure different timestamps (SQLite datetime has second precision)
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let _p2 = upsert_project(&pool, "/home/user/newer-project")
        .await
        .expect("Failed to create newer-project");

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let _p3 = upsert_project(&pool, "/home/user/newest-project")
        .await
        .expect("Failed to create newest-project");

    let open_projects = list_open_projects(&pool)
        .await
        .expect("Failed to list projects");

    // Should be in reverse chronological order (newest first)
    assert_eq!(open_projects[0].name, "newest-project");
    assert_eq!(open_projects[1].name, "newer-project");
    assert_eq!(open_projects[2].name, "old-project");
}

#[tokio::test]
async fn close_project_sets_is_open_to_zero() {
    let pool = temp_db().await;

    let project = upsert_project(&pool, "/home/user/my-project")
        .await
        .expect("Failed to create project");

    close_project(&pool, project.id)
        .await
        .expect("Failed to close project");

    let open_projects = list_open_projects(&pool)
        .await
        .expect("Failed to list projects");

    assert_eq!(open_projects.len(), 0);
}

#[tokio::test]
async fn upsert_document_stores_content() {
    let pool = temp_db().await;

    let project = upsert_project(&pool, "/home/user/my-project")
        .await
        .expect("Failed to create project");

    upsert_document(&pool, project.id, "prd", "# Product Requirements\n\nGoal: Build a thing")
        .await
        .expect("Failed to upsert document");

    let docs = get_documents(&pool, project.id)
        .await
        .expect("Failed to get documents");

    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].doc_type, "prd");
    assert_eq!(docs[0].content, "# Product Requirements\n\nGoal: Build a thing");
}

#[tokio::test]
async fn upsert_document_updates_existing() {
    let pool = temp_db().await;

    let project = upsert_project(&pool, "/home/user/my-project")
        .await
        .expect("Failed to create project");

    // Insert initial content
    upsert_document(&pool, project.id, "prd", "Version 1")
        .await
        .expect("Failed to upsert document");

    // Update with new content
    upsert_document(&pool, project.id, "prd", "Version 2")
        .await
        .expect("Failed to update document");

    let docs = get_documents(&pool, project.id)
        .await
        .expect("Failed to get documents");

    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].content, "Version 2");
}

#[tokio::test]
async fn get_documents_returns_multiple_doc_types() {
    let pool = temp_db().await;

    let project = upsert_project(&pool, "/home/user/my-project")
        .await
        .expect("Failed to create project");

    upsert_document(&pool, project.id, "prd", "PRD content")
        .await
        .expect("Failed to upsert PRD");

    upsert_document(&pool, project.id, "spec", "Spec content")
        .await
        .expect("Failed to upsert spec");

    let docs = get_documents(&pool, project.id)
        .await
        .expect("Failed to get documents");

    assert_eq!(docs.len(), 2);

    let doc_types: Vec<String> = docs.iter().map(|d| d.doc_type.clone()).collect();
    assert!(doc_types.contains(&"prd".to_string()));
    assert!(doc_types.contains(&"spec".to_string()));
}

#[tokio::test]
async fn get_documents_returns_empty_for_project_without_docs() {
    let pool = temp_db().await;

    let project = upsert_project(&pool, "/home/user/my-project")
        .await
        .expect("Failed to create project");

    let docs = get_documents(&pool, project.id)
        .await
        .expect("Failed to get documents");

    assert_eq!(docs.len(), 0);
}

#[tokio::test]
async fn project_name_extracted_from_folder_path() {
    let pool = temp_db().await;

    // Only test Unix-style paths (this code runs on macOS/Linux servers)
    // Windows paths would be handled by Windows servers
    let test_cases = vec![
        ("/home/user/my-project", "my-project"),
        ("/var/www/site", "site"),
        ("/single", "single"),
        ("/path/with-dashes", "with-dashes"),
    ];

    for (path, expected_name) in test_cases {
        let project = upsert_project(&pool, path)
            .await
            .expect("Failed to create project");
        assert_eq!(project.name, expected_name, "Failed for path: {}", path);
    }
}

#[tokio::test]
async fn upsert_project_is_idempotent() {
    let pool = temp_db().await;

    // Call upsert multiple times with same path
    let p1 = upsert_project(&pool, "/home/user/same-project")
        .await
        .expect("Failed first upsert");

    let p2 = upsert_project(&pool, "/home/user/same-project")
        .await
        .expect("Failed second upsert");

    let p3 = upsert_project(&pool, "/home/user/same-project")
        .await
        .expect("Failed third upsert");

    // All should have the same ID
    assert_eq!(p1.id, p2.id);
    assert_eq!(p2.id, p3.id);

    // Should only have one project in the database
    let all_projects = list_open_projects(&pool)
        .await
        .expect("Failed to list projects");
    assert_eq!(all_projects.len(), 1);
}
