use oracle_fusion_erp_catalog_mcp::db::{CatalogColumn, CatalogReference, CatalogTable, Database};
use rusqlite::Connection;
use tempfile::tempdir;

fn table(name: &str, description: &str) -> CatalogTable {
    CatalogTable {
        module: "FINANCIALS".to_owned(),
        table_name: name.to_owned(),
        description: Some(description.to_owned()),
        columns: vec![CatalogColumn {
            column_name: "ID".to_owned(),
            data_type: "NUMBER".to_owned(),
            length: Some(18),
            nullable: false,
            description: Some("Identifier".to_owned()),
        }],
        ..CatalogTable::default()
    }
}

#[test]
fn active_version_queries_return_structure_and_search_results() {
    let db = Database::in_memory().expect("database");
    let version_id = db.create_version("26B", true).expect("version");
    db.upsert_catalog_table(
        version_id,
        &table("AP_INVOICES_ALL", "Accounts payable invoice headers"),
    )
    .expect("table");

    let structure = db
        .table_structure("ap_invoices_all")
        .expect("structure query")
        .expect("structure");
    assert_eq!(structure.table.table_name, "AP_INVOICES_ALL");
    assert_eq!(structure.columns[0].column_name, "ID");

    let matches = db.search_tables("invoices", 10).expect("search query");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].table_name, "AP_INVOICES_ALL");
}

#[test]
fn queries_are_limited_to_the_active_release() {
    let db = Database::in_memory().expect("database");
    let old_id = db.create_version("26B", false).expect("old version");
    db.upsert_catalog_table(old_id, &table("OLD_TABLE", "Old release"))
        .expect("old table");
    let active_id = db.create_version("26C", true).expect("active version");
    db.upsert_catalog_table(active_id, &table("CURRENT_TABLE", "Current release"))
        .expect("current table");

    let tables = db
        .list_modules_and_tables(None, 100, 0)
        .expect("list query");
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].table_name, "CURRENT_TABLE");
    assert!(db
        .table_structure("OLD_TABLE")
        .expect("structure query")
        .is_none());
}

#[test]
fn discovers_columns_relationships_and_releases() {
    let db = Database::in_memory().expect("database");
    let version_id = db.create_version("26B", true).expect("version");
    db.upsert_catalog_table(
        version_id,
        &table("PO_HEADERS_ALL", "Purchase order headers"),
    )
    .expect("parent table");

    let mut lines = table("PO_LINES_ALL", "Purchase order lines");
    lines.columns.push(CatalogColumn {
        column_name: "HEADER_ID".to_owned(),
        data_type: "NUMBER".to_owned(),
        length: None,
        nullable: false,
        description: Some("Purchase order header identifier".to_owned()),
    });
    lines.references.push(CatalogReference {
        target_table: "PO_HEADERS_ALL".to_owned(),
        source_column: "HEADER_ID".to_owned(),
        target_column: None,
        constraint_name: None,
    });
    db.upsert_catalog_table(version_id, &lines)
        .expect("child table");

    let by_column = db
        .find_tables_by_column("header_id", None, 10)
        .expect("column lookup");
    assert_eq!(by_column[0].table_name, "PO_LINES_ALL");

    let column_matches = db
        .search_columns("header identifier", None, 10)
        .expect("column search");
    assert_eq!(column_matches[0].column.column_name, "HEADER_ID");

    let related = db
        .find_related_tables("PO_HEADERS_ALL", 1, 10)
        .expect("related tables");
    assert_eq!(related[0].table.table_name, "PO_LINES_ALL");
    assert_eq!(related[0].depth, 1);
    assert_eq!(related[0].target_column, None);

    let joins = db
        .suggest_joins("PO_HEADERS_ALL", "PO_LINES_ALL")
        .expect("join suggestions");
    assert_eq!(joins.len(), 1);
    assert_eq!(joins[0].target_column, None);

    let releases = db.list_versions().expect("release listing");
    assert_eq!(releases[0].version.release_code, "26B");
    assert!(releases[0].version.active);
    assert_eq!(releases[0].table_count, 2);
    assert_eq!(releases[0].modules, vec!["FINANCIALS"]);
}

#[test]
fn migrates_existing_foreign_key_schema_without_losing_target_columns() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("catalog.sqlite");
    let connection = Connection::open(&path).expect("legacy database");
    connection
        .execute_batch(
            "CREATE TABLE versions (
                 id INTEGER PRIMARY KEY,
                 release_code TEXT NOT NULL UNIQUE,
                 synced_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                 active_bool INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE tables (
                 id INTEGER PRIMARY KEY,
                 version_id INTEGER NOT NULL,
                 module TEXT NOT NULL,
                 table_name TEXT NOT NULL,
                 description TEXT,
                 source_url TEXT,
                 object_type TEXT,
                 UNIQUE(version_id, table_name)
             );
             CREATE TABLE foreign_key_references (
                 id INTEGER PRIMARY KEY,
                 source_table_id INTEGER NOT NULL,
                 source_column TEXT NOT NULL,
                 target_table_id INTEGER NOT NULL,
                 target_column TEXT NOT NULL,
                 constraint_name TEXT
             );
             INSERT INTO versions (id, release_code, active_bool)
                 VALUES (1, '26B', 1);
             INSERT INTO tables (id, version_id, module, table_name)
                 VALUES (1, 1, 'FINANCIALS', 'CHILD_TABLE'),
                        (2, 1, 'FINANCIALS', 'PARENT_TABLE');
             INSERT INTO foreign_key_references
                 (id, source_table_id, source_column, target_table_id, target_column)
                 VALUES (1, 1, 'PARENT_ID', 2, 'ID');",
        )
        .expect("create legacy schema");
    drop(connection);

    let database = Database::open(&path).expect("migrate database");
    let structure = database
        .table_structure("CHILD_TABLE")
        .expect("structure query")
        .expect("child table");
    assert_eq!(structure.outgoing_references.len(), 1);
    assert_eq!(
        structure.outgoing_references[0].target_column.as_deref(),
        Some("ID")
    );
}
