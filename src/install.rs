use crate::paths;
use anyhow::{anyhow, Context, Result};
use serde_json::{Map, Value};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SERVER_NAME: &str = "oracle-fusion-erp-catalog-mcp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Cursor,
    ClaudeCode,
    Codex,
    OpenCode,
}

impl Agent {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "cursor" => Ok(Self::Cursor),
            "claude-code" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::OpenCode),
            _ => Err(anyhow!(
                "unsupported agent {value}; use cursor, claude-code, codex, opencode, or all"
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Cursor => "Cursor",
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex CLI",
            Self::OpenCode => "OpenCode",
        }
    }
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub agents: Vec<Agent>,
    pub binary: PathBuf,
    pub database: PathBuf,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteStatus {
    Changed,
    Unchanged,
}

pub fn help() -> &'static str {
    "Register the MCP server in agent configuration files\n\n\
Usage: oracle-fusion-erp-catalog-mcp install AGENT [OPTIONS]\n\n\
AGENT:\n  cursor | claude-code | codex | opencode | all\n\n\
Options:\n  --binary PATH       Server executable (default: current executable)\n  --database PATH     SQLite database (default: platform data directory)\n  --dry-run           Show changes without writing files\n  -h, --help          Show this help\n  -V, --version       Show the version"
}

pub fn parse_args(args: &[String]) -> Result<InstallOptions> {
    let agent_name = args
        .first()
        .filter(|value| !value.starts_with('-'))
        .ok_or_else(|| anyhow!("install requires an agent; use --help for usage"))?;
    let agents = if agent_name == "all" {
        vec![
            Agent::Cursor,
            Agent::ClaudeCode,
            Agent::Codex,
            Agent::OpenCode,
        ]
    } else {
        vec![Agent::parse(agent_name)?]
    };
    let mut binary = env::current_exe().context("could not determine current executable")?;
    let mut database = paths::database_path()?;
    let mut dry_run = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--binary" => {
                index += 1;
                binary = PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| anyhow!("--binary requires a path"))?,
                );
            }
            "--database" => {
                index += 1;
                database = PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| anyhow!("--database requires a path"))?,
                );
            }
            "--dry-run" => dry_run = true,
            "-h" | "--help" => return Err(anyhow!("help requested")),
            "-V" | "--version" => return Err(anyhow!("version requested")),
            value => return Err(anyhow!("unsupported argument: {value}")),
        }
        index += 1;
    }
    if !binary.is_file() {
        return Err(anyhow!("binary path is not a file: {}", binary.display()));
    }
    binary = fs::canonicalize(binary)?;
    if database.is_relative() {
        database = env::current_dir()?.join(database);
    }
    Ok(InstallOptions {
        agents,
        binary,
        database,
        dry_run,
    })
}

pub fn install(options: &InstallOptions) -> Result<Vec<String>> {
    let home = home_dir()?;
    let mut changed = Vec::new();
    for agent in &options.agents {
        let path = config_path(*agent, &home);
        let status = match agent {
            Agent::Cursor | Agent::ClaudeCode => update_json_config(&path, *agent, options)?,
            Agent::Codex => update_toml_config(&path, options)?,
            Agent::OpenCode => update_opencode_config(&path, options)?,
        };
        if status == WriteStatus::Changed {
            changed.push(path.display().to_string());
        }
        println!(
            "{}: {} {}",
            agent.label(),
            if options.dry_run {
                "would update"
            } else {
                "updated"
            },
            path.display()
        );
    }
    if changed.is_empty() {
        println!("No configuration changes needed.");
    }
    Ok(changed)
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

fn config_path(agent: Agent, home: &Path) -> PathBuf {
    match agent {
        Agent::Cursor => home.join(".cursor/mcp.json"),
        Agent::ClaudeCode => home.join(".claude.json"),
        Agent::Codex => home.join(".codex/config.toml"),
        Agent::OpenCode => home.join(".config/opencode/opencode.json"),
    }
}

fn command_value(options: &InstallOptions) -> Value {
    serde_json::json!({
        "type": "stdio",
        "command": options.binary.to_string_lossy(),
        "args": [],
        "env": { "ORACLE_MCP_DATABASE": options.database.to_string_lossy() }
    })
}

fn update_json_config(path: &Path, _agent: Agent, options: &InstallOptions) -> Result<WriteStatus> {
    let mut root = read_json(path)?;
    let object = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} must contain a JSON object", path.display()))?;
    let key = "mcpServers";
    let servers = object
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow!("{key} in {} must be an object", path.display()))?;
    let desired = command_value(options);
    let changed = servers.get(SERVER_NAME) != Some(&desired);
    if changed {
        servers.insert(SERVER_NAME.to_owned(), desired);
    }
    write_json(path, &root, options.dry_run, changed)
}

fn update_opencode_config(path: &Path, options: &InstallOptions) -> Result<WriteStatus> {
    let mut root = read_json(path)?;
    let object = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} must contain a JSON object", path.display()))?;
    let mcp = object
        .entry("mcp")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("mcp in {} must be an object", path.display()))?;
    let servers = mcp
        .entry("servers")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow!("mcp.servers in {} must be an object", path.display()))?;
    let desired = serde_json::json!({
        "type": "local",
        "command": [options.binary.to_string_lossy()],
        "environment": { "ORACLE_MCP_DATABASE": options.database.to_string_lossy() },
        "enabled": true
    });
    let changed = servers.get(SERVER_NAME) != Some(&desired);
    if changed {
        servers.insert(SERVER_NAME.to_owned(), desired);
    }
    write_json(path, &root, options.dry_run, changed)
}

fn update_toml_config(path: &Path, options: &InstallOptions) -> Result<WriteStatus> {
    let original = read_text(path)?;
    let mut document = if original.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        original
            .parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("could not parse {}", path.display()))?
    };
    let command = options.binary.to_string_lossy().into_owned();
    let database = options.database.to_string_lossy().into_owned();
    let current_command = document
        .get("mcp_servers")
        .and_then(|servers| servers.get(SERVER_NAME))
        .and_then(|server| server.get("command"))
        .and_then(toml_edit::Item::as_str);
    let current_database = document
        .get("mcp_servers")
        .and_then(|servers| servers.get(SERVER_NAME))
        .and_then(|server| server.get("env"))
        .and_then(|env| env.get("ORACLE_MCP_DATABASE"))
        .and_then(toml_edit::Item::as_str);
    let changed =
        current_command != Some(command.as_str()) || current_database != Some(database.as_str());
    if changed {
        let servers = require_table(&mut document, "mcp_servers", "mcp_servers")?;
        let server = require_table(servers, SERVER_NAME, "mcp server")?;
        server.insert("command", toml_edit::value(command));
        server.insert("args", toml_edit::value(toml_edit::Array::new()));
        let env = require_table(server, "env", "env")?;
        env.insert("ORACLE_MCP_DATABASE", toml_edit::value(database));
    }
    write_text(path, &document.to_string(), options.dry_run, changed)
}

fn require_table<'a>(
    parent: &'a mut toml_edit::Table,
    key: &str,
    label: &str,
) -> Result<&'a mut toml_edit::Table> {
    let item = parent.entry(key).or_insert(toml_edit::table());
    item.as_table_mut()
        .ok_or_else(|| anyhow!("{label} must be a table"))
}

fn read_json(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("could not parse {}", path.display()))
}

fn read_text(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))
}

fn write_json(path: &Path, value: &Value, dry_run: bool, changed: bool) -> Result<WriteStatus> {
    let content = serde_json::to_string_pretty(value)? + "\n";
    write_text(path, &content, dry_run, changed)
}

fn write_text(path: &Path, content: &str, dry_run: bool, changed: bool) -> Result<WriteStatus> {
    if !changed || dry_run {
        return Ok(if changed {
            WriteStatus::Changed
        } else {
            WriteStatus::Unchanged
        });
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("configuration path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| anyhow!("system clock is before Unix epoch: {error}"))?
        .as_nanos();
    let filename = path
        .file_name()
        .ok_or_else(|| anyhow!("configuration path has no filename: {}", path.display()))?;
    let temp = parent.join(format!(".{}.{}.tmp", filename.to_string_lossy(), stamp));
    {
        let mut file = fs::File::create(&temp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    {
        let mode = if path.exists() {
            fs::metadata(path)?.permissions().mode()
        } else {
            0o600
        };
        fs::set_permissions(&temp, fs::Permissions::from_mode(mode))?;
    }
    fs::rename(&temp, path).with_context(|| format!("could not replace {}", path.display()))?;
    Ok(WriteStatus::Changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn options(dir: &Path, dry_run: bool) -> InstallOptions {
        let binary = dir.join("server");
        fs::write(&binary, b"binary").expect("binary");
        InstallOptions {
            agents: vec![Agent::Cursor],
            binary,
            database: dir.join("data.sqlite"),
            dry_run,
        }
    }

    #[test]
    fn json_merge_is_idempotent_and_preserves_unrelated_entries() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("mcp.json");
        fs::write(&path, r#"{"other":{"keep":true}}"#).expect("config");
        let options = options(dir.path(), false);
        assert_eq!(
            update_json_config(&path, Agent::Cursor, &options).expect("write"),
            WriteStatus::Changed
        );
        let first = fs::read_to_string(&path).expect("read");
        assert_eq!(
            update_json_config(&path, Agent::Cursor, &options).expect("write"),
            WriteStatus::Unchanged
        );
        assert_eq!(first, fs::read_to_string(&path).expect("read"));
        let value: Value = serde_json::from_str(&first).expect("json");
        assert_eq!(value["other"]["keep"], true);
    }

    #[test]
    fn dry_run_does_not_create_config() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("nested/mcp.json");
        let options = options(dir.path(), true);
        assert_eq!(
            write_json(&path, &command_value(&options), true, true).expect("dry run"),
            WriteStatus::Changed
        );
        assert!(!path.exists());
    }

    #[test]
    fn supports_claude_json_config() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("claude.json");
        let options = options(dir.path(), false);
        update_json_config(&path, Agent::ClaudeCode, &options).expect("write");
        let value: Value =
            serde_json::from_str(&fs::read_to_string(path).expect("read")).expect("json");
        assert_eq!(value["mcpServers"][SERVER_NAME]["type"], "stdio");
    }

    #[test]
    fn supports_codex_toml_config() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "# keep me\n[other]\nkeep = true\n").expect("config");
        let options = options(dir.path(), false);
        update_toml_config(&path, &options).expect("write");
        let value: toml::Value = fs::read_to_string(path)
            .expect("read")
            .parse()
            .expect("toml");
        assert_eq!(
            value["mcp_servers"][SERVER_NAME]["env"]["ORACLE_MCP_DATABASE"].as_str(),
            Some(options.database.to_string_lossy().as_ref())
        );
        assert_eq!(value["other"]["keep"].as_bool(), Some(true));
        let text = fs::read_to_string(dir.path().join("config.toml")).expect("read");
        assert!(text.contains("# keep me"));
    }

    #[test]
    fn rejects_a_codex_server_table_that_is_not_a_table() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "mcp_servers = \"nope\"\n").expect("config");
        let error =
            update_toml_config(&path, &options(dir.path(), false)).expect_err("non-table config");
        assert!(error.to_string().contains("must be a table"));
    }

    #[test]
    fn supports_opencode_config() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("opencode.json");
        let options = options(dir.path(), false);
        update_opencode_config(&path, &options).expect("write");
        let value: Value =
            serde_json::from_str(&fs::read_to_string(path).expect("read")).expect("json");
        assert_eq!(value["mcp"]["servers"][SERVER_NAME]["type"], "local");
    }

    #[test]
    fn parses_all_agents_and_rejects_invalid_binary() {
        let dir = tempdir().expect("tempdir");
        let server = dir.path().join("server");
        fs::write(&server, b"binary").expect("binary");
        let args = vec![
            "all".to_owned(),
            "--binary".to_owned(),
            server.to_string_lossy().into_owned(),
            "--dry-run".to_owned(),
        ];
        let parsed = parse_args(&args).expect("install options");
        assert_eq!(parsed.agents.len(), 4);
        assert!(parsed.dry_run);

        let invalid = vec![
            "cursor".to_owned(),
            "--binary".to_owned(),
            "/missing".to_owned(),
        ];
        assert!(parse_args(&invalid).is_err());
    }
}
