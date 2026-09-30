# Oracle Fusion ERP Catalog MCP

Oracle Fusion ERP Catalog MCP is a Rust MCP server for querying the Oracle
Fusion Cloud Financials and SCM technical dictionary through SQLite and FTS5.

## Why

Oracle ERP schemas are large, versioned, and difficult to search from an agent.
This server stores the published table metadata locally, keeps releases
separate, and exposes exact lookup, lexical search, structure, and join tools
over MCP. Official source patterns are documented in
[`docs/oracle-table-sources.md`](docs/oracle-table-sources.md).

## Disclaimer

Oracle and Oracle Fusion are trademarks of Oracle Corporation. This project
is not affiliated with, endorsed by, or sponsored by Oracle Corporation.

The source code is MIT licensed; content and metadata retrieved from Oracle Help
Center remain subject to Oracle's terms.

## Install

Install the latest release:

```sh
curl -fsSL \
  https://raw.githubusercontent.com/thegreatyamori/oracle-fusion-erp-catalog-mcp/main/scripts/install.sh \
  | sh
```

If you already installed it, update the binary with:

```sh
oracle-fusion-erp-catalog-mcp update
```

## Set up your agent

Run the command for the agent you use, then restart that agent:

| Agent | Setup |
| --- | --- |
| Cursor | `oracle-fusion-erp-catalog-mcp install cursor --binary "$HOME/.local/bin/oracle-fusion-erp-catalog-mcp"` |
| Claude Code | `oracle-fusion-erp-catalog-mcp install claude-code --binary "$HOME/.local/bin/oracle-fusion-erp-catalog-mcp"` |
| Codex CLI | `oracle-fusion-erp-catalog-mcp install codex --binary "$HOME/.local/bin/oracle-fusion-erp-catalog-mcp"` |
| OpenCode | `oracle-fusion-erp-catalog-mcp install opencode --binary "$HOME/.local/bin/oracle-fusion-erp-catalog-mcp"` |
| All agents | `oracle-fusion-erp-catalog-mcp install all --binary "$HOME/.local/bin/oracle-fusion-erp-catalog-mcp"` |

Use `--dry-run` to preview changes. Set `ORACLE_MCP_DATABASE` to override the
default database location. Without an override, the database is stored as
`catalog.sqlite` in the platform user-data directory:

- macOS: `~/Library/Application Support/oracle-fusion-erp-catalog-mcp/catalog.sqlite`
- Linux: `${XDG_DATA_HOME:-~/.local/share}/oracle-fusion-erp-catalog-mcp/catalog.sqlite`
- Windows: `%LOCALAPPDATA%\oracle-fusion-erp-catalog-mcp\catalog.sqlite`

Install the pre-generated catalog for a release:

```sh
oracle-fusion-erp-catalog-mcp catalog install --release 26B
```

This downloads the published SQLite catalog. Use `sync` only when generating
or refreshing a catalog directly from Oracle.

## Sync

Synchronize Financials, SCM, and HCM from the Oracle Help Center:

```sh
oracle-fusion-erp-catalog-mcp sync --release 26B --module all
```

Use `--module financials|scm|hcm|all`, `--no-activate`, or `--replace` as
needed. This command is intended for catalog generation and can take a long
time because it downloads the Oracle guides.
The parsed result is cached by release and module under the application data
directory, so retrying the same synchronization reuses completed modules.
Set `ORACLE_MCP_SYNC_PARALLELISM` to control extraction concurrency; the
default is `2`. Set it to `1` to minimize network and memory pressure.
Synchronization activates newer releases and removes the previous active
release after success; a missing module can be merged into an existing release.
The database path is controlled by `ORACLE_MCP_DATABASE`.

## MCP usage

With no subcommand, the installed binary speaks JSON-RPC 2.0 over stdin/stdout.
Logs go to stderr, while stdout is reserved for MCP messages. Configure your
agent using the setup command above, or try the protocol directly:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | oracle-fusion-erp-catalog-mcp
```

Available tools:

| Tool | Description |
| --- | --- |
| `list_modules_and_tables` | Lists tables from the active release. Optional module filter, limit, and offset. |
| `search_table_structure` | Searches for a table and returns its technical structure. |
| `suggest_joins` | Returns direct relationships between two tables. |
| `find_tables_by_column` | Finds tables containing a column. |
| `search_columns` | Searches columns by name or description. |
| `find_related_tables` | Finds tables related through foreign keys. |
| `list_releases` | Lists synchronized releases and identifies the active one. |

## Setup dev environment

Install stable Rust, then verify changes from a checkout. Operational and user
commands above use the installed binary.

```sh
make verify
make compile
make check
make test
```

For optional local coverage, install `cargo-llvm-cov` and run:

```sh
cargo llvm-cov --workspace --all-features --lcov --output-path lcov.info
```

