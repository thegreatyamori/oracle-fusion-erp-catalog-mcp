use futures::{stream::FuturesUnordered, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use oracle_fusion_erp_catalog_mcp::catalog;
use oracle_fusion_erp_catalog_mcp::db::Database;
use oracle_fusion_erp_catalog_mcp::install;
use oracle_fusion_erp_catalog_mcp::paths;
use oracle_fusion_erp_catalog_mcp::sync::{self, OracleExtractor, OracleModule};
use oracle_fusion_erp_catalog_mcp::sync_cache;
use oracle_fusion_erp_catalog_mcp::update;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{env, io, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Semaphore;

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            println!("{}", cli_help());
            Ok(())
        }
        Some("-V" | "--version" | "version") => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("update") if matches!(args.get(1).map(String::as_str), Some("-h" | "--help")) => {
            println!("{}", update_help());
            Ok(())
        }
        Some("update") => update::run(&args[1..])
            .await
            .map_err(|error| io::Error::other(error.to_string())),
        Some("catalog") if matches!(args.get(1).map(String::as_str), Some("-h" | "--help")) => {
            println!("{}", catalog_help());
            Ok(())
        }
        Some("catalog") => run_catalog_command(&args[1..])
            .await
            .map_err(|error| io::Error::other(error.to_string())),
        Some("sync") if matches!(args.get(1).map(String::as_str), Some("-h" | "--help")) => {
            println!("{}", sync_help());
            Ok(())
        }
        Some("sync") if matches!(args.get(1).map(String::as_str), Some("-V" | "--version")) => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("sync") => run_sync_command(&args[1..])
            .await
            .map_err(|error| io::Error::other(error.to_string())),
        Some("install") => run_install_command(&args[1..]),
        None => run_mcp().await,
        Some(command) => {
            eprintln!("unsupported command: {command}");
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported command: {command}"),
            ))
        }
    }
}

fn cli_help() -> &'static str {
    "Oracle Fusion ERP Catalog MCP\n\n\
Usage:\n  oracle-fusion-erp-catalog-mcp [OPTIONS]\n  oracle-fusion-erp-catalog-mcp install AGENT [OPTIONS]\n  oracle-fusion-erp-catalog-mcp update [OPTIONS]\n  oracle-fusion-erp-catalog-mcp catalog install --release RELEASE\n  oracle-fusion-erp-catalog-mcp sync --release RELEASE [OPTIONS]\n\n\
Options:\n  -h, --help       Show this help\n  -V, --version    Show the version\n\n\
With no command, the process starts the MCP server."
}

fn catalog_help() -> &'static str {
    "Install a pre-generated Oracle catalog into SQLite\n\n\
Usage:\n  oracle-fusion-erp-catalog-mcp catalog install --release RELEASE\n\n\
The catalog is downloaded from GitHub Releases and validated before installation."
}

fn update_help() -> &'static str {
    "Update the installed binary\n\n\
Usage:\n  oracle-fusion-erp-catalog-mcp update [OPTIONS]\n\n\
Options:\n  --check                 Check for an update without installing it\n  --version VERSION       Install a specific release\n  -h, --help              Show this help"
}

fn run_install_command(args: &[String]) -> io::Result<()> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        println!("{}", install::help());
        return Ok(());
    }
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-V" | "--version"))
    {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let options = install::parse_args(args)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    install::install(&options)
        .map(|_| ())
        .map_err(|error| io::Error::other(error.to_string()))
}

fn sync_help() -> &'static str {
    "Synchronize an Oracle release into SQLite\n\n\
Usage:\n  oracle-fusion-erp-catalog-mcp sync --release RELEASE [OPTIONS]\n\n\
Options:\n  --release RELEASE              Oracle release, such as 26B\n  --module financials|scm|hcm|all Module to synchronize\n  --no-activate                   Keep the synchronized release inactive\n  --replace                       Delete the existing target release before syncing\n  -h, --help                      Show this help\n  -V, --version                   Show the version"
}

async fn run_catalog_command(args: &[String]) -> anyhow::Result<()> {
    if args.first().map(String::as_str) != Some("install") {
        return Err(anyhow::anyhow!("catalog requires the install subcommand"));
    }

    let mut release = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--release" => {
                index += 1;
                release = Some(
                    args.get(index)
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(|| anyhow::anyhow!("--release requires a value"))?
                        .to_ascii_uppercase(),
                );
            }
            "-h" | "--help" => {
                println!("{}", catalog_help());
                return Ok(());
            }
            value => return Err(anyhow::anyhow!("unsupported catalog argument {value}")),
        }
        index += 1;
    }

    let release = release.ok_or_else(|| anyhow::anyhow!("catalog install requires --release"))?;
    let database_path = paths::database_path()?;
    let installed = catalog::install(&release, &database_path).await?;
    if installed {
        eprintln!("installed catalog release {release}");
    } else {
        eprintln!("catalog release {release} is already installed");
    }
    Ok(())
}

async fn run_sync_command(args: &[String]) -> anyhow::Result<()> {
    let (release, modules, activate, replace) = parse_sync_args(args)?;
    let database_path = paths::database_path()?;
    let database = Database::open(&database_path)?;
    if replace {
        database.delete_version_by_release(&release)?;
    }

    let sources: Vec<_> = modules
        .into_iter()
        .map(|module| sync::OracleSource::help_center(module, &release))
        .collect::<anyhow::Result<_>>()?;
    let mut cached_modules = Vec::with_capacity(sources.len());
    let mut missing_sources = Vec::new();
    for source in sources {
        if sync_cache::load(&source)?.is_some() {
            eprintln!("using cached {} {}", source.module.label(), source.release);
            cached_modules.push((source.clone(), sync_cache::cache_path(&source)?));
        } else {
            missing_sources.push(source);
        }
    }

    let extractor = Arc::new(OracleExtractor::new()?);
    let semaphore = Arc::new(Semaphore::new(sync_parallelism()?));
    let mut extraction_tasks = FuturesUnordered::new();
    for source in missing_sources {
        let extractor = Arc::clone(&extractor);
        let semaphore = Arc::clone(&semaphore);
        extraction_tasks.push(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .map_err(|_| anyhow::anyhow!("sync parallelism limiter was closed"))?;
            let progress = ProgressBar::new_spinner();
            progress.set_style(
                ProgressStyle::with_template(&format!(
                    "{} {{spinner}} {{msg}}",
                    source.module.label()
                ))
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
            );
            progress.enable_steady_tick(std::time::Duration::from_millis(100));
            progress.set_message(format!("processing {}", source.index_url));
            let extracted = extractor
                .extract_with_progress(&source, Some(&progress))
                .await;
            progress.finish_and_clear();
            let tables = extracted?;
            let table_count = tables.len();
            let cache_path = sync_cache::store(&source, &tables)?;
            eprintln!(
                "cached {} {} catalog entries",
                source.module.label(),
                table_count
            );
            Ok::<_, anyhow::Error>((source, cache_path))
        });
    }
    while let Some(task) = extraction_tasks.next().await {
        cached_modules.push(task?);
    }
    cached_modules.sort_by_key(|(source, _)| source.module.label().to_owned());

    let mut import_progress = None;
    let sync_result = sync::synchronize_cached_modules(
        &database,
        &release,
        &cached_modules
            .iter()
            .map(|(source, path)| (source.clone(), path.as_path()))
            .collect::<Vec<_>>(),
        activate,
        |module, completed, total| {
            if completed == 0 {
                let progress = ProgressBar::new(total as u64);
                progress.set_style(
                    ProgressStyle::with_template("{prefix} {bar:40.cyan/blue} {pos}/{len}")
                        .unwrap_or_else(|_| ProgressStyle::default_bar()),
                );
                progress.set_prefix(module.to_owned());
                import_progress = Some(progress);
            }
            if let Some(progress) = import_progress.as_ref() {
                progress.set_position(completed as u64);
                if completed >= total {
                    progress.finish_and_clear();
                    import_progress = None;
                }
            }
        },
    );
    if let Some(progress) = import_progress {
        progress.finish_and_clear();
    }
    let version_id = sync_result?;
    eprintln!("synchronized release {release} as version {version_id}");
    Ok(())
}

fn sync_parallelism() -> anyhow::Result<usize> {
    let configured = env::var("ORACLE_MCP_SYNC_PARALLELISM").ok();
    parse_sync_parallelism(configured.as_deref())
}

fn parse_sync_parallelism(value: Option<&str>) -> anyhow::Result<usize> {
    let parallelism = value
        .unwrap_or("2")
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("ORACLE_MCP_SYNC_PARALLELISM must be a positive integer"))?;
    if parallelism == 0 {
        return Err(anyhow::anyhow!(
            "ORACLE_MCP_SYNC_PARALLELISM must be a positive integer"
        ));
    }
    Ok(parallelism)
}

fn parse_sync_args(args: &[String]) -> anyhow::Result<(String, Vec<OracleModule>, bool, bool)> {
    let mut release = None;
    let mut modules = vec![
        OracleModule::Financials,
        OracleModule::Scm,
        OracleModule::Hcm,
    ];
    let mut activate = true;
    let mut replace = false;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--release" => {
                index += 1;
                let value = args
                    .get(index)
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("--release requires a value"))?;
                release = Some(value.to_ascii_uppercase());
            }
            "--module" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--module requires a value"))?;
                modules = match value.to_ascii_lowercase().as_str() {
                    "financials" => vec![OracleModule::Financials],
                    "scm" => vec![OracleModule::Scm],
                    "hcm" => vec![OracleModule::Hcm],
                    "all" => vec![
                        OracleModule::Financials,
                        OracleModule::Scm,
                        OracleModule::Hcm,
                    ],
                    _ => {
                        return Err(anyhow::anyhow!(
                            "unsupported module {value}; use financials, scm, hcm, or all"
                        ))
                    }
                };
            }
            "--no-activate" => activate = false,
            "--replace" => replace = true,
            "-h" | "--help" => {
                eprintln!(
                    "Usage: cargo run -- sync --release RELEASE [--module financials|scm|hcm|all] [--no-activate] [--replace]"
                );
                return Err(anyhow::anyhow!("help requested"));
            }
            value => return Err(anyhow::anyhow!("unsupported argument: {value}")),
        }
        index += 1;
    }

    let release = release.ok_or_else(|| anyhow::anyhow!("sync requires --release RELEASE"))?;
    Ok((release, modules, activate, replace))
}

async fn run_mcp() -> io::Result<()> {
    tokio::spawn(async {
        update::notify_if_available().await;
    });
    let database_path =
        paths::database_path().map_err(|error| io::Error::other(error.to_string()))?;
    let database = match Database::open(&database_path) {
        Ok(database) => database,
        Err(error) => {
            eprintln!("could not open SQLite: {error}");
            return Err(io::Error::other(error.to_string()));
        }
    };
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::BufWriter::new(tokio::io::stdout());

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(request) => handle_request(&database, request),
            Err(error) => Some(error_response(
                None,
                -32700,
                "invalid JSON",
                Some(json!({ "detail": error.to_string() })),
            )),
        };
        let Some(response) = response else {
            continue;
        };
        let encoded = serde_json::to_string(&response).unwrap_or_else(|_| {
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"internal error"}}"#
                .to_owned()
        });
        stdout.write_all(encoded.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }
    Ok(())
}

fn handle_request(database: &Database, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
    request.id.as_ref()?;
    if request.jsonrpc != "2.0" {
        return Some(error_response(
            request.id,
            -32600,
            "jsonrpc must be 2.0",
            None,
        ));
    }
    Some(match request.method.as_str() {
        "initialize" => success(
            request.id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "oracle-fusion-erp-catalog-mcp", "version": env!("CARGO_PKG_VERSION") }
            }),
        ),
        "tools/list" => success(request.id, tool_definitions()),
        "tools/call" => match call_tool(database, &request.params) {
            Ok(result) => success(request.id, result),
            Err(error) => error_response(request.id, -32602, &error, None),
        },
        _ => error_response(request.id, -32601, "method not supported", None),
    })
}

fn tool_definitions() -> Value {
    json!({
        "tools": [
            {
                "name": "list_modules_and_tables",
                "description": "Lists tables from the active release. Filter by module and page with limit and offset.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "module": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 500 },
                        "offset": { "type": "integer", "minimum": 0 }
                    }
                }
            },
            {
                "name": "search_table_structure",
                "description": "Searches for an exact table or by text and returns its technical structure.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "suggest_joins",
                "description": "Returns the exact relationships between two tables.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "table_a": { "type": "string" },
                        "table_b": { "type": "string" }
                    },
                    "required": ["table_a", "table_b"]
                }
            },
            {
                "name": "find_tables_by_column",
                "description": "Finds active-release tables containing an exact or prefixed column name.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "column": { "type": "string" },
                        "module": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                    },
                    "required": ["column"]
                }
            },
            {
                "name": "search_columns",
                "description": "Searches active-release column names and descriptions.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "module": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "find_related_tables",
                "description": "Finds tables related to a table through foreign-key paths.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "table": { "type": "string" },
                        "max_depth": { "type": "integer", "minimum": 1, "maximum": 3 },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                    },
                    "required": ["table"]
                }
            },
            {
                "name": "list_releases",
                "description": "Lists synchronized Oracle releases and identifies the active release.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            }
        ]
    })
}

fn call_tool(database: &Database, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("tools/call requires name")?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match name {
        "list_modules_and_tables" => {
            let module = arguments.get("module").and_then(Value::as_str);
            let limit = arguments
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .clamp(1, 500) as usize;
            let offset = arguments
                .get("offset")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(1_000_000) as usize;
            let tables = database
                .list_modules_and_tables(module, limit, offset)
                .map_err(|error| error.to_string())?;
            Ok(tool_result(
                serde_json::to_value(tables).map_err(|e| e.to_string())?,
            ))
        }
        "search_table_structure" => {
            let query = required_string(&arguments, "name")?;
            if let Some(structure) = database
                .table_structure(&query)
                .map_err(|error| error.to_string())?
            {
                return Ok(tool_result(
                    serde_json::to_value(structure).map_err(|e| e.to_string())?,
                ));
            }
            let limit = arguments
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(10)
                .clamp(1, 100) as usize;
            let matches = database
                .search_tables(&query, limit)
                .map_err(|error| error.to_string())?;
            Ok(tool_result(
                serde_json::to_value(matches).map_err(|e| e.to_string())?,
            ))
        }
        "suggest_joins" => {
            let left = required_string(&arguments, "table_a")?;
            let right = required_string(&arguments, "table_b")?;
            let references = database
                .suggest_joins(&left, &right)
                .map_err(|error| error.to_string())?;
            Ok(tool_result(
                serde_json::to_value(references).map_err(|e| e.to_string())?,
            ))
        }
        "find_tables_by_column" => {
            let column = required_string(&arguments, "column")?;
            let module = arguments.get("module").and_then(Value::as_str);
            let limit = bounded_limit(&arguments);
            let tables = database
                .find_tables_by_column(&column, module, limit)
                .map_err(|error| error.to_string())?;
            Ok(tool_result(
                serde_json::to_value(tables).map_err(|e| e.to_string())?,
            ))
        }
        "search_columns" => {
            let query = required_string(&arguments, "query")?;
            let module = arguments.get("module").and_then(Value::as_str);
            let limit = bounded_limit(&arguments);
            let columns = database
                .search_columns(&query, module, limit)
                .map_err(|error| error.to_string())?;
            Ok(tool_result(
                serde_json::to_value(columns).map_err(|e| e.to_string())?,
            ))
        }
        "find_related_tables" => {
            let table = required_string(&arguments, "table")?;
            let max_depth = arguments
                .get("max_depth")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, 3) as usize;
            let related = database
                .find_related_tables(&table, max_depth, bounded_limit(&arguments))
                .map_err(|error| error.to_string())?;
            Ok(tool_result(
                serde_json::to_value(related).map_err(|e| e.to_string())?,
            ))
        }
        "list_releases" => {
            let releases = database
                .list_versions()
                .map_err(|error| error.to_string())?;
            Ok(tool_result(
                serde_json::to_value(releases).map_err(|e| e.to_string())?,
            ))
        }
        _ => Err(format!("tool not supported: {name}")),
    }
}

fn bounded_limit(arguments: &Value) -> usize {
    arguments
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .clamp(1, 100) as usize
}

fn required_string(arguments: &Value, key: &str) -> Result<String, String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("required argument: {key}"))
}

fn tool_result(value: Value) -> Value {
    let structured_content = if value.is_object() {
        value.clone()
    } else {
        json!({ "data": value })
    };
    json!({
        "content": [{ "type": "text", "text": serde_json::to_string(&value).unwrap_or_default() }],
        "structuredContent": structured_content,
        "isError": false
    })
}

fn success(id: Option<Value>, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

fn error_response(
    id: Option<Value>,
    code: i32,
    message: &str,
    data: Option<Value>,
) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_owned(),
            data,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sync_options() {
        let args = vec![
            "--release".to_owned(),
            "26b".to_owned(),
            "--module".to_owned(),
            "scm".to_owned(),
            "--no-activate".to_owned(),
        ];
        let (release, modules, activate, replace) = parse_sync_args(&args).expect("sync options");
        assert_eq!(release, "26B");
        assert_eq!(modules, vec![OracleModule::Scm]);
        assert!(!activate);
        assert!(!replace);
    }

    #[test]
    fn all_syncs_financials_scm_and_hcm() {
        let args = vec![
            "--release".to_owned(),
            "26B".to_owned(),
            "--module".to_owned(),
            "all".to_owned(),
        ];
        let (_, modules, _, _) = parse_sync_args(&args).expect("sync options");
        assert_eq!(
            modules,
            vec![
                OracleModule::Financials,
                OracleModule::Scm,
                OracleModule::Hcm
            ]
        );
    }

    #[test]
    fn limits_sync_parallelism_to_positive_values() {
        assert_eq!(parse_sync_parallelism(None).expect("default"), 2);
        assert_eq!(parse_sync_parallelism(Some("1")).expect("serial"), 1);
        assert!(parse_sync_parallelism(Some("0")).is_err());
        assert!(parse_sync_parallelism(Some("many")).is_err());
    }

    #[test]
    fn requires_sync_release() {
        let error = parse_sync_args(&[]).expect_err("missing release");
        assert_eq!(error.to_string(), "sync requires --release RELEASE");
    }

    #[test]
    fn exposes_cli_help_and_version_text() {
        assert!(cli_help().contains("oracle-fusion-erp-catalog-mcp"));
        assert!(cli_help().contains(" update [OPTIONS]"));
        assert!(update_help().contains("--check"));
        assert!(sync_help().contains("--replace"));
        assert!(
            env!("CARGO_PKG_VERSION")
                .split('.')
                .filter(|part| !part.is_empty())
                .count()
                >= 3
        );
    }

    #[test]
    fn wraps_non_object_structured_content() {
        let result = tool_result(json!([{ "release_code": "26B" }]));

        assert!(result["structuredContent"].is_object());
        assert_eq!(
            result["structuredContent"]["data"][0]["release_code"],
            "26B"
        );
    }
}
