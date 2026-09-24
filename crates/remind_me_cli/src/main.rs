#[cfg(test)]
#[path = "../../remind_me_core/src/test_env.rs"]
mod test_env;

use remind_me_api::ApiServer;
use remind_me_core::db::queries;
use remind_me_core::{
    entity, reminders, stats, updater, wiki, wiki_import, Database, EntityInput, MemoryAddInput,
    MemoryListInput, MemorySearchInput, ResponseFormat, LIST_LIMIT_MAX, LIST_LIMIT_MIN,
};
use remind_me_mcp::McpServer;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::PathBuf;

fn get_target_config_paths() -> Vec<(PathBuf, &'static str, &'static str)> {
    let mut targets = Vec::new();
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));

    #[cfg(target_os = "windows")]
    {
        let appdata = env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData").join("Roaming"));
        targets.push((
            appdata.join("Claude").join("claude_desktop_config.json"),
            "Claude Desktop",
            "claude-desktop",
        ));
    }
    #[cfg(not(target_os = "windows"))]
    {
        targets.push((
            home.join(".config")
                .join("Claude")
                .join("claude_desktop_config.json"),
            "Claude Desktop",
            "claude-desktop",
        ));
    }

    targets.push((
        home.join(".gemini")
            .join("antigravity")
            .join("mcp_config.json"),
        "Antigravity",
        "antigravity",
    ));
    targets.push((home.join(".cursor").join("mcp.json"), "Cursor", "cursor"));
    targets.push((
        home.join(".mcp").join("config.json"),
        "Codex / Generic MCP Client",
        "mcp",
    ));

    targets
}

const CONFIGURE_USAGE: &str = "\
Usage: rusty-remind-me configure [--node-id ID --hub-url URL] [--peer-port N]
                                 [--sync-interval SECS] [--db-path PATH]
                                 [--default-format json|markdown]

Writes the MCP server entry for every client this tool knows about. With
--node-id and --hub-url it also writes the sync environment, so a synced node
needs one command rather than a hand-edited config per client.

The sync secret is read from REMIND_ME_SYNC_SECRET in the environment. There
is deliberately no --secret flag: argv is world-readable through /proc on
Linux and lands in shell history, and a token that grants access to the whole
memory database should not go there.";

/// Parsed `configure` flags.
#[derive(Debug, Default, PartialEq, Eq)]
struct ConfigureArgs {
    node_id: Option<String>,
    hub_url: Option<String>,
    peer_port: Option<u16>,
    sync_interval: Option<u64>,
    db_path: Option<String>,
    default_format: Option<String>,
}

fn parse_configure_args(args: &[String]) -> Result<ConfigureArgs, String> {
    let mut parsed = ConfigureArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            flag @ ("--node-id" | "--hub-url" | "--peer-port" | "--sync-interval" | "--db-path"
            | "--default-format") => {
                let value = args.get(i + 1).ok_or_else(|| {
                    format!("Error: {} expects a value.\n{}", flag, CONFIGURE_USAGE)
                })?;
                match flag {
                    "--node-id" => parsed.node_id = Some(value.clone()),
                    "--hub-url" => parsed.hub_url = Some(value.clone()),
                    "--db-path" => parsed.db_path = Some(value.clone()),
                    // Validated here rather than at read time: a typo written
                    // into every MCP client config would otherwise be silently
                    // ignored by the server and look like the flag did nothing.
                    "--default-format" => match value.as_str() {
                        "json" | "markdown" => parsed.default_format = Some(value.clone()),
                        other => {
                            return Err(format!(
                            "Error: --default-format expects `json` or `markdown`, got {:?}.\n{}",
                            other, CONFIGURE_USAGE
                        ))
                        }
                    },
                    "--peer-port" => {
                        parsed.peer_port = Some(value.parse().map_err(|_| {
                            format!(
                                "Error: --peer-port expects a number 0-65535, got {:?}.",
                                value
                            )
                        })?)
                    }
                    _ => {
                        parsed.sync_interval = Some(value.parse().map_err(|_| {
                            format!(
                                "Error: --sync-interval expects a number of seconds, got {:?}.",
                                value
                            )
                        })?)
                    }
                }
                i += 2;
            }
            // Rejected by name rather than falling into "unknown flag", so the
            // reason is the reason -- someone reaching for --secret should
            // learn why it does not exist, not that it is misspelled.
            "--secret" | "--sync-secret" => {
                return Err(format!(
                    "Error: there is no {} flag. Pass the secret through the \
                     environment instead:\n\n    \
                     REMIND_ME_SYNC_SECRET=... rusty-remind-me configure ...\n\n\
                     argv is world-readable through /proc and is kept in shell \
                     history; the environment of a process is neither.",
                    args[i]
                ));
            }
            other => {
                return Err(format!(
                    "Error: unknown flag {:?}.\n{}",
                    other, CONFIGURE_USAGE
                ));
            }
        }
    }

    // Sync turns on only when node id, hub URL and secret are ALL present
    // (`remind_me_core::sync::sync_enabled`). A half-configured node is
    // therefore a node with sync silently off, which is the failure this
    // whole session kept running into -- so say so at the point the mistake
    // is made, rather than letting it look configured.
    let secret = std::env::var(remind_me_core::sync::SYNC_SECRET_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());
    let wants_sync = parsed.node_id.is_some() || parsed.hub_url.is_some() || secret.is_some();
    if wants_sync {
        let mut missing = Vec::new();
        if parsed.node_id.is_none() {
            missing.push("--node-id");
        }
        if parsed.hub_url.is_none() {
            missing.push("--hub-url");
        }
        if secret.is_none() {
            missing.push("REMIND_ME_SYNC_SECRET");
        }
        if !missing.is_empty() {
            return Err(format!(
                "Error: sync needs a node id, a hub URL and a secret, and {} \
                 {} missing. Sync would be silently disabled otherwise.\n{}",
                missing.join(" + "),
                if missing.len() == 1 { "is" } else { "are" },
                CONFIGURE_USAGE
            ));
        }
    }

    Ok(parsed)
}

fn configure_mcp_clients(parsed: &ConfigureArgs) -> Result<(), Box<dyn std::error::Error>> {
    let current_exe = env::current_exe()?;
    let exe_str = current_exe.to_string_lossy().to_string();
    let db_path = match &parsed.db_path {
        Some(p) => PathBuf::from(p),
        // Was `~/.remind_me/remind_me.db` -- underscore, and a filename neither
        // the runtime path nor the reference used. Now the one resolver, so a
        // client configured here and a bare `rusty-remind-me` open one file.
        None => remind_me_core::db::resolve_db_path(),
    };

    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut env_map = json!({
        "REMIND_ME_DB_PATH": db_path.to_string_lossy()
    });

    // Only written when asked for. An entry that always pinned the default
    // would freeze it into every client config, so a later change to the
    // shipped default could never reach anyone who had run `configure`.
    if let Some(format) = &parsed.default_format {
        env_map[remind_me_mcp::DEFAULT_FORMAT_ENV] = json!(format);
    }

    // `parse_configure_args` has already refused any partial combination, so
    // reaching here with a node id means the hub URL and secret are present
    // too -- there is no half-configured entry to write.
    if let Some(node_id) = &parsed.node_id {
        let secret = std::env::var(remind_me_core::sync::SYNC_SECRET_ENV).unwrap_or_default();
        let hub_url = parsed.hub_url.clone().unwrap_or_default();
        env_map[remind_me_core::sync::NODE_ID_ENV] = json!(node_id);
        env_map[remind_me_core::sync::HUB_URL_ENV] = json!(hub_url);
        env_map[remind_me_core::sync::SYNC_SECRET_ENV] = json!(secret);
        env_map[remind_me_core::sync::PEER_PORT_ENV] = json!(parsed
            .peer_port
            .unwrap_or(remind_me_core::sync::DEFAULT_PEER_PORT)
            .to_string());
        env_map[remind_me_core::sync::SYNC_INTERVAL_ENV] = json!(parsed
            .sync_interval
            .unwrap_or(remind_me_core::sync::DEFAULT_SYNC_INTERVAL_SECS)
            .to_string());
    }

    for (config_path, name, client_slug) in get_target_config_paths() {
        // Built per target rather than once: REMIND_ME_CLIENT is the writer
        // the hub records against every memory, so "which app was this typed
        // into" is answerable in /stats. Left unset it is `unknown` for
        // everything, which is the same as not having the field.
        let mut env_for_target = env_map.clone();
        env_for_target[remind_me_core::sync::CLIENT_ENV] = json!(client_slug);
        let server_entry = json!({
            "command": exe_str,
            "args": ["server"],
            "env": env_for_target,
        });

        if let Some(parent) = config_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let mut data: Value = json!({ "mcpServers": {} });

        if config_path.exists() {
            if let Ok(content) = fs::read_to_string(&config_path) {
                if let Ok(parsed) = serde_json::from_str::<Value>(&content) {
                    if parsed.is_object() {
                        data = parsed;
                    }
                }
            }
        }

        if !data.get("mcpServers").is_some_and(|v| v.is_object()) {
            data["mcpServers"] = json!({});
        }

        data["mcpServers"]["rusty-remind-me"] = server_entry;

        if let Ok(json_str) = serde_json::to_string_pretty(&data) {
            if fs::write(&config_path, json_str).is_ok() {
                println!("✔ Configured {}: {}", name, config_path.display());
            }
        }
    }

    if let Some(node_id) = &parsed.node_id {
        println!(
            "\nSync configured: node {:?} -> {}",
            node_id,
            parsed.hub_url.as_deref().unwrap_or("")
        );
        // Named, not printed: the written files hold the secret because a
        // client has to read it, but echoing it to a terminal puts it in
        // scrollback and any terminal-logging setup for no benefit -- whoever
        // ran this already had it in their environment.
        println!(
            "The secret was taken from {} and written into each config above.",
            remind_me_core::sync::SYNC_SECRET_ENV
        );
    } else {
        println!("\nSync not configured (no --node-id). Pass --node-id and --hub-url, with REMIND_ME_SYNC_SECRET set, to enable it.");
    }

    println!("\nSetup complete! Restart your client application to activate rusty-remind-me.");
    Ok(())
}

/// Parsed form of `rusty-remind-me list [--limit N] [--category C] [--json]`.
///
/// The flag set is deliberately the reference's, not this crate's tool input's.
/// [`MemoryListInput`] also carries `tags`, `source`, `offset` and
/// `include_sensitive`, but `remind_me_mcp/cli.py`'s `list_p` exposes only
/// `--limit`, `--category` and `--json` — and a CLI that accepts flags the
/// reference rejects is the same drop-in divergence in the other direction.
#[derive(Debug)]
struct ListArgs {
    limit: usize,
    category: Option<String>,
    as_json: bool,
}

/// Parse `list`'s flags, or return the message to print to stderr.
///
/// Returns `Err` rather than exiting so this is testable without a process
/// boundary; `main` is what turns the message into an exit code.
fn parse_list_args(args: &[String]) -> Result<ListArgs, String> {
    let mut parsed = ListArgs {
        // Matches `list_p.add_argument("--limit", type=int, default=20)`.
        limit: 20,
        category: None,
        as_json: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => {
                parsed.as_json = true;
                i += 1;
            }
            "--limit" | "--category" => {
                let flag = &args[i];
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| format!("Error: {} expects a value.\n{}", flag, LIST_USAGE))?;
                if flag == "--limit" {
                    let n: usize = value.parse().map_err(|_| {
                        format!("Error: --limit expects a number, got {:?}.", value)
                    })?;
                    // The reference's MemoryListInput bounds `limit`, so an
                    // out-of-range value is a validation error there. Rejecting
                    // here rather than deferring to `list_memories`, which
                    // clamps silently — a caller who asked for 500 should be
                    // told they got 100, not quietly handed a short page.
                    if !(LIST_LIMIT_MIN..=LIST_LIMIT_MAX).contains(&n) {
                        return Err(format!(
                            "Error: --limit must be between {} and {}, got {}.",
                            LIST_LIMIT_MIN, LIST_LIMIT_MAX, n
                        ));
                    }
                    parsed.limit = n;
                } else {
                    parsed.category = Some(value.clone());
                }
                i += 2;
            }
            other => {
                return Err(format!("Error: unknown flag {:?}.\n{}", other, LIST_USAGE));
            }
        }
    }
    Ok(parsed)
}

const LIST_USAGE: &str = "Usage: rusty-remind-me list [--limit N] [--category CATEGORY] [--json]";

const ADD_USAGE: &str = "Usage: rusty-remind-me add <content> [--category CATEGORY] [--tags a,b,c]";

const SEARCH_USAGE: &str = "Usage: rusty-remind-me search <query> [--limit N] [--json]";

/// Parsed form of `rusty-remind-me add <content> [--category C] [--tags a,b]`.
///
/// Same principle as [`ListArgs`]: the flag set is the reference's `add_p`, not
/// [`MemoryAddInput`]'s full field list. The tool input also carries `subject`,
/// `predicate`, `object`, `entities`, `metadata` and `sensitive`; the reference
/// exposes none of them from the command line, and accepting flags it rejects
/// would be the drop-in divergence running the other way.
#[derive(Debug, PartialEq, Eq)]
struct AddArgs {
    content: String,
    category: String,
    tags: Vec<String>,
}

/// Parsed form of `rusty-remind-me search <query> [--limit N] [--json]`.
#[derive(Debug, PartialEq, Eq)]
struct SearchArgs {
    query: String,
    limit: usize,
    as_json: bool,
}

/// Split a `--tags` value the way the reference's `_split_tags` does.
///
/// Comma-separated, trimmed, blanks dropped — so `--tags ""` is an empty list
/// rather than one empty tag, matching `MemoryAddInput.tags`'s own default.
fn split_tags(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Collect the positional words of a subcommand, rejecting unknown flags.
///
/// # Why this exists at all
///
/// `add` and `search` previously did `args[2..].join(" ")` — every argument,
/// flags included, became the content or the query. `argparse` rejects an
/// unknown flag; a `join` cannot, because every argument is valid text. So
/// `add "note" --category engineering` stored the *string*
/// `"note --category engineering"` in category `general`, exited 0, and printed
/// a success line. The only way to see the bug was to read the row back (#216).
///
/// # Multiple positionals
///
/// The reference declares a single positional, so `add one two` is an error
/// there. Here the words are joined, which is what this CLI already did and
/// what unquoted shell input produces. That is a deliberate superset: it keeps
/// working input working, while the flag handling below closes the actual gap.
///
/// `--` ends flag parsing, so content that genuinely starts with `--` is still
/// expressible.
fn collect_positional<F>(
    args: &[String],
    usage: &'static str,
    mut on_flag: F,
) -> Result<String, String>
where
    F: FnMut(&str, &[String], &mut usize) -> Result<bool, String>,
{
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    let mut flags_done = false;
    while i < args.len() {
        let arg = &args[i];
        if flags_done || !arg.starts_with("--") {
            words.push(arg.clone());
            i += 1;
            continue;
        }
        if arg == "--" {
            flags_done = true;
            i += 1;
            continue;
        }
        if !on_flag(arg, args, &mut i)? {
            return Err(format!("Error: unknown flag {:?}.\n{}", arg, usage));
        }
    }
    if words.is_empty() {
        return Err(usage.to_string());
    }
    Ok(words.join(" "))
}

/// Read the value following a flag, or explain what was missing.
fn flag_value(
    args: &[String],
    i: usize,
    flag: &str,
    usage: &'static str,
) -> Result<String, String> {
    args.get(i + 1)
        .cloned()
        .ok_or_else(|| format!("Error: {} expects a value.\n{}", flag, usage))
}

fn parse_add_args(args: &[String]) -> Result<AddArgs, String> {
    // Matches `add_p.add_argument("--category", default="general")` and
    // `--tags` defaulting to "" -> [].
    let mut category = "general".to_string();
    let mut tags: Vec<String> = Vec::new();

    let content = collect_positional(args, ADD_USAGE, |flag, args, i| match flag {
        "--category" => {
            category = flag_value(args, *i, flag, ADD_USAGE)?;
            *i += 2;
            Ok(true)
        }
        "--tags" => {
            tags = split_tags(&flag_value(args, *i, flag, ADD_USAGE)?);
            *i += 2;
            Ok(true)
        }
        _ => Ok(false),
    })?;

    Ok(AddArgs {
        content,
        category,
        tags,
    })
}

fn parse_search_args(args: &[String]) -> Result<SearchArgs, String> {
    // `search_p.add_argument("--limit", type=int, default=20)`.
    let mut limit = 20usize;
    let mut as_json = false;

    let query = collect_positional(args, SEARCH_USAGE, |flag, args, i| match flag {
        "--json" => {
            as_json = true;
            *i += 1;
            Ok(true)
        }
        "--limit" => {
            let raw = flag_value(args, *i, flag, SEARCH_USAGE)?;
            let n: usize = raw
                .parse()
                .map_err(|_| format!("Error: --limit expects a number, got {:?}.", raw))?;
            // Bounded like `list`'s, and for the same reason: `search_memories`
            // would otherwise clamp silently, and a caller who asked for 500
            // should be told they got 100 rather than handed a short page and
            // left to assume the vault is small.
            if !(LIST_LIMIT_MIN..=LIST_LIMIT_MAX).contains(&n) {
                return Err(format!(
                    "Error: --limit must be between {} and {}, got {}.",
                    LIST_LIMIT_MIN, LIST_LIMIT_MAX, n
                ));
            }
            limit = n;
            *i += 2;
            Ok(true)
        }
        _ => Ok(false),
    })?;

    Ok(SearchArgs {
        query,
        limit,
        as_json,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // First, before argv or the database. The slow-call watchdog dumps thread
    // stacks by re-executing this binary as a tracer child, and everything
    // done ahead of this line would be done again in every such child --
    // including opening the database. A no-op unless the `stack-dumps`
    // feature is on, and it never returns when this process *is* the child.
    remind_me_core::watchdog::install_stack_dump_hook();

    let args: Vec<String> = env::args().collect();
    // Resolution lives in the core crate so `configure` below writes the same
    // path this opens. They disagreed before: this defaulted to `remind_me.db`
    // in the *current directory*, so the same command from two directories was
    // two databases, while `configure` wrote `~/.remind_me/remind_me.db` -- and
    // the reference used `~/.remind-me/memory.db`, a third answer (#218).
    let db_path = remind_me_core::db::resolve_db_path();
    if let Some(parent) = db_path.parent() {
        // The default now lives under a home-directory folder that need not
        // exist yet. Without this, a first run fails to open rather than
        // creating the store, which the old cwd-relative default never hit.
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let db = Database::open(&db_path)?;

    if args.len() < 2 || args[1] == "server" || args[1] == "mcp" {
        // Non-blocking, and deliberately not inside McpServer::new: that
        // constructor is also what the test suite uses to build a server
        // per-test, and a background `git fetch` on every one of those
        // would be slow, network-dependent, and racy across parallel tests.
        updater::start_background_check();
        // Unconditional, unlike the folder watcher: reminders have no enable
        // switch, only a poll interval. Without this the scheduler would be
        // code nothing ever runs, and a reminder would only ever fire if
        // someone happened to call a tool.
        let scheduler = remind_me_core::scheduler::start_scheduler_for(&db.conn());
        // Conditional, unlike the scheduler: the watcher has an explicit
        // enable switch, so this is `None` unless REMIND_ME_WATCH_DIRS names a
        // usable directory. Until #203 this call did not exist at all, and
        // `scan_once` ran only when a test invoked it — the status surface
        // reported a configured watcher that was never going to scan anything.
        let watcher = remind_me_core::watcher::start_watcher_for(&db.conn());
        // Conditional like the watcher: `None` unless
        // REMIND_ME_PROMOTION_INTERVAL is set. The refinement ladder's
        // candidate queries are pull-only without this, so a backlog can grow
        // indefinitely with nothing ever mentioning it (#208).
        let nudge = remind_me_core::promotion::start_nudge_for(&db.conn());
        // Conditional like the watcher/nudge: `None` unless node id, hub URL
        // and secret are all configured. A background loop like the three
        // above, not something `McpServer` owns (#316's plugin work needed
        // this to keep syncing memories written through the CLI or a sibling
        // `rusty-remind-me api` process, not only ones written over MCP) --
        // started here alongside it instead, and stopped in the same join
        // block below.
        let mut sync = remind_me_core::sync::SyncWorker::from_env(db_path.clone());
        let server = McpServer::new(db);
        let result = server.run_stdio_loop();
        // All joined before the database goes out of scope, so an in-flight
        // poll, scan or backlog count cannot still be running while the handle
        // is torn down underneath it.
        if let Some(scheduler) = scheduler {
            scheduler.stop();
        }
        if let Some(watcher) = watcher {
            watcher.stop();
        }
        if let Some(nudge) = nudge {
            nudge.stop();
        }
        if let Some(sync) = sync.as_mut() {
            sync.stop();
        }
        result?;
    } else {
        match args[1].as_str() {
            "configure" | "setup" => {
                let configure_args = match parse_configure_args(&args[2..]) {
                    Ok(parsed) => parsed,
                    Err(message) => {
                        eprintln!("{}", message);
                        std::process::exit(1);
                    }
                };
                configure_mcp_clients(&configure_args)?;
            }
            "api" => {
                let port_arg = args.get(2).cloned().unwrap_or_else(|| "8080".to_string());
                let port: u16 = port_arg.parse().map_err(|_| {
                    format!("invalid port {:?}; expected a number 0-65535", port_arg)
                })?;
                // Optional third argv, matching the reference's `--ui-host`
                // (default 127.0.0.1) -- the reference's `--serve-ui` binds
                // loopback unless told otherwise, and a deployment reaching
                // this server from another host (a Tailscale IP, a LAN
                // interface) needs the same escape hatch. `port` stayed
                // argv-only for the same reason `main`'s own comment above
                // draws the line between "api" (argv) and "server"/"remote"
                // (env) -- adding host as a second positional keeps that
                // convention rather than introducing a new env var for one
                // flag.
                let host = args
                    .get(3)
                    .cloned()
                    .unwrap_or_else(|| "127.0.0.1".to_string());
                let addr = format!("{}:{}", host, port);

                // #90: refuse a double start for the same DB, matching the
                // reference's `--serve-ui` guard in `__main__.py`. An
                // in-memory database has nowhere to put a PID file -- that
                // is not an error, it just means this protection is
                // unavailable, same as an in-memory DB having no backup
                // directory (`status::server_status`).
                let pid_path = match remind_me_core::pid::pid_file_path(&db.conn()) {
                    Ok(path) => Some(path),
                    Err(remind_me_core::pid::PidError::InMemory) => {
                        eprintln!(
                            "Warning: in-memory database has no on-disk location for a PID \
                             file; double-start protection is unavailable."
                        );
                        None
                    }
                    Err(e) => return Err(e.into()),
                };
                if let Some(path) = &pid_path {
                    let status = remind_me_core::pid::dashboard_status(path);
                    if status.running {
                        eprintln!(
                            "Dashboard is already running at {} (PID {}). Stop it first or use \
                             a different port with `rusty-remind-me api <port>`.",
                            status.url.as_deref().unwrap_or("unknown"),
                            status
                                .pid
                                .map(|p| p.to_string())
                                .unwrap_or_else(|| "unknown".to_string()),
                        );
                        std::process::exit(1);
                    }
                    remind_me_core::pid::write_pid_file(path, &host, port)?;
                }

                let scheduler = remind_me_core::scheduler::start_scheduler_for(&db.conn());
                // A long-lived daemon like "server", so it can carry the sync
                // worker just as well -- see the comment on "server"'s own
                // `SyncWorker::from_env` call for why this no longer lives
                // inside `McpServer` specifically.
                let mut sync = remind_me_core::sync::SyncWorker::from_env(db_path.clone());
                let api_server = ApiServer::new(db);
                let result = api_server.run(&addr);
                if let Some(scheduler) = scheduler {
                    scheduler.stop();
                }
                if let Some(sync) = sync.as_mut() {
                    sync.stop();
                }
                if let Some(path) = &pid_path {
                    remind_me_core::pid::remove_pid_file(path);
                }
                result?;
            }
            "remote" => {
                // The only place this crate ever touches the async side of
                // the workspace: remind_me_remote::run_blocking owns
                // spinning up its own tokio runtime and blocks this thread
                // on it, so this crate stays synchronous like every other
                // subcommand here -- no `async fn`, no tokio dependency of
                // its own. Bind host/port and the connector token are
                // resolved from the environment inside run_blocking
                // (REMIND_ME_REMOTE_HOST/_PORT/_TOKEN), matching how "api"
                // above takes its port from argv while "server" takes its
                // config from the environment.
                //
                // Also long-lived like "server"/"api", so it gets the same
                // sync worker treatment -- this is still an MCP server, just
                // over Streamable HTTP instead of stdio, and previously got a
                // SyncWorker implicitly through `McpServer::new` the same way
                // "server" did. Stopped after `run_blocking` returns, same as
                // the others; `run_blocking` blocks for the connector's whole
                // life, so there is nothing to interleave it with.
                let mut sync = remind_me_core::sync::SyncWorker::from_env(db_path.clone());
                let server = McpServer::new(db);
                let result = remind_me_remote::run_blocking(server);
                if let Some(sync) = sync.as_mut() {
                    sync.stop();
                }
                result?;
            }
            "search" => {
                let search_args = match parse_search_args(&args[2..]) {
                    Ok(parsed) => parsed,
                    Err(message) => {
                        eprintln!("{}", message);
                        std::process::exit(1);
                    }
                };
                let search_input = MemorySearchInput {
                    strategy: Default::default(),
                    include_sensitive: false,
                    query: search_args.query,
                    category: None,
                    tags: None,
                    limit: search_args.limit,
                    token_budget: 800,
                    response_format: if search_args.as_json {
                        ResponseFormat::Json
                    } else {
                        ResponseFormat::Markdown
                    },
                    include_dormant: false,
                    min_vitality: 0.0,
                    verbose: false,
                    expand_entities: false,
                    include_neighbors: false,
                    expand_co_retrieval: false,
                    // Only `search_with_expansions` assembles a bootstrap, and
                    // this path calls `search_memories`. Set true here it
                    // would be silently ignored, so it stays false until the
                    // CLI grows a flag and the call to match.
                    bootstrap: false,
                };
                let conn = db.conn();
                let results = queries::search_memories(&conn, &search_input)?;
                match search_input.response_format {
                    // JSON keeps the whole result -- scores, per-signal
                    // components, the lot -- because that is what a script
                    // piping this wants.
                    ResponseFormat::Json => {
                        println!("{}", serde_json::to_string_pretty(&results)?)
                    }
                    // The reference renders search hits through the same
                    // `_fmt_memories` it uses for `list`, which drops the
                    // scores. Matching that rather than inventing a richer
                    // layout: this text is what a person reads, and `list` and
                    // `search` looking different in the same binary would be a
                    // divergence invented here.
                    ResponseFormat::Markdown => {
                        let memories: Vec<_> = results.into_iter().map(|r| r.memory).collect();
                        println!("{}", reminders::render_memories_markdown(&memories))
                    }
                }
            }
            "add" => {
                let add_args = match parse_add_args(&args[2..]) {
                    Ok(parsed) => parsed,
                    Err(message) => {
                        eprintln!("{}", message);
                        std::process::exit(1);
                    }
                };
                let add_input = MemoryAddInput {
                    sensitive: false,
                    content: add_args.content,
                    category: add_args.category,
                    tags: add_args.tags,
                    source: "cli".to_string(),
                    metadata: serde_json::json!({}),
                    subject: None,
                    predicate: None,
                    object: None,
                    entities: vec![],
                };
                let conn = db.conn();
                let mem = queries::add_memory(&conn, add_input)?;
                println!("Added memory: {}", mem.id);
            }
            "list" => {
                // Browse by filter, no ranking -- the counterpart to `search`,
                // and the reason both exist: `search` answers "what do I know
                // about X", `list` answers "show me the X slice".
                let list_args = match parse_list_args(&args[2..]) {
                    Ok(parsed) => parsed,
                    Err(message) => {
                        eprintln!("{}", message);
                        std::process::exit(1);
                    }
                };
                let list_input = MemoryListInput {
                    category: list_args.category,
                    limit: list_args.limit,
                    response_format: if list_args.as_json {
                        ResponseFormat::Json
                    } else {
                        ResponseFormat::Markdown
                    },
                    ..Default::default()
                };
                let conn = db.conn();
                let page = queries::list_memories(&conn, &list_input)?;
                match list_input.response_format {
                    // The reference's `_fmt_memories` JSON branch carries
                    // `count`/`memories`/`total`; `MemoryListResult` also
                    // carries the pagination cursor, so it is serialized whole
                    // rather than reshaped into a strictly smaller payload.
                    ResponseFormat::Json => {
                        println!("{}", serde_json::to_string_pretty(&page)?)
                    }
                    ResponseFormat::Markdown => {
                        println!(
                            "{}",
                            reminders::render_memory_page_markdown(&page.memories, page.total)
                        )
                    }
                }
            }
            "get" => {
                if args.len() < 3 {
                    eprintln!("Usage: rusty-remind-me get <id>");
                    std::process::exit(1);
                }
                let id = &args[2];
                let conn = db.conn();
                if let Some(mem) = queries::get_memory_by_id(&conn, id)? {
                    println!("{}", serde_json::to_string_pretty(&mem)?);
                } else {
                    eprintln!("Memory not found: {}", id);
                }
            }
            "entity" => {
                if args.len() < 3 {
                    eprintln!("Usage: rusty-remind-me entity <name> [kind]");
                    std::process::exit(1);
                }
                let name = args[2].clone();
                let kind = args.get(3).cloned();
                let conn = db.conn();
                let ent = entity::upsert_entity(
                    &conn,
                    &EntityInput {
                        name,
                        kind,
                        aliases: vec![],
                    },
                )?;
                println!("{}", serde_json::to_string_pretty(&ent)?);
            }
            "wiki-write" => {
                if args.len() < 5 {
                    eprintln!("Usage: rusty-remind-me wiki-write <slug> <title> <content>");
                    std::process::exit(1);
                }
                let slug = &args[2];
                let title = &args[3];
                let content = &args[4];
                let conn = db.conn();
                let page = wiki::write_wiki_page(&conn, slug, title, content, "")?;
                println!("Saved wiki page: {}", page.slug);
            }
            "wiki-read" => {
                if args.len() < 3 {
                    eprintln!("Usage: rusty-remind-me wiki-read <slug>");
                    std::process::exit(1);
                }
                let slug = &args[2];
                let conn = db.conn();
                if let Some(page) = wiki::get_wiki_page(&conn, slug)? {
                    println!("{}", serde_json::to_string_pretty(&page)?);
                } else {
                    eprintln!("Wiki page not found: {}", slug);
                }
            }
            "wiki-import" => {
                if args.len() < 3 {
                    eprintln!("Usage: rusty-remind-me wiki-import <dir>");
                    eprintln!("Imports every .md file in <dir> into the wiki. Pairs with");
                    eprintln!("`dbs export-wiki --out-dir <dir>` from daily-backup-system.");
                    std::process::exit(1);
                }
                let dir = PathBuf::from(&args[2]);
                let conn = db.conn();
                let report = wiki_import::import_wiki_dir(&conn, &dir, true)?;
                for page in &report.imported {
                    println!("{}  <- {}", page.slug, page.path);
                }
                for (path, reason) in &report.skipped {
                    eprintln!("skipped {}: {}", path, reason);
                }
                println!(
                    "Imported {} page(s), skipped {}.",
                    report.imported.len(),
                    report.skipped.len()
                );
            }
            "stats" => {
                let conn = db.conn();
                println!("{}", serde_json::to_string_pretty(&stats::collect(&conn)?)?);
            }
            cmd => {
                eprintln!("Unknown subcommand: {}. Available: configure, api, remote, server, search, add, list, get, entity, wiki-write, wiki-read, wiki-import, stats", cmd);
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_flags_matches_the_reference_defaults() {
        let parsed = parse_list_args(&args(&[])).expect("bare `list` is valid");
        assert_eq!(parsed.limit, 20);
        assert_eq!(parsed.category, None);
        assert!(!parsed.as_json);
    }

    #[test]
    fn every_flag_parses_together() {
        let parsed =
            parse_list_args(&args(&["--limit", "5", "--category", "work", "--json"])).unwrap();
        assert_eq!(parsed.limit, 5);
        assert_eq!(parsed.category.as_deref(), Some("work"));
        assert!(parsed.as_json);
    }

    #[test]
    fn flag_order_does_not_matter() {
        let parsed =
            parse_list_args(&args(&["--json", "--category", "work", "--limit", "5"])).unwrap();
        assert_eq!(parsed.limit, 5);
        assert_eq!(parsed.category.as_deref(), Some("work"));
        assert!(parsed.as_json);
    }

    #[test]
    fn a_non_numeric_limit_is_rejected() {
        let err = parse_list_args(&args(&["--limit", "lots"])).unwrap_err();
        assert!(err.contains("--limit expects a number"), "got: {}", err);
    }

    /// The boundary that matters: `list_memories` clamps, so without this the
    /// out-of-range value would be silently honoured as something else.
    #[test]
    fn an_out_of_range_limit_is_rejected_rather_than_clamped() {
        for bad in [0, LIST_LIMIT_MAX + 1] {
            let err = parse_list_args(&args(&["--limit", &bad.to_string()])).unwrap_err();
            assert!(
                err.contains("must be between"),
                "limit {} should be refused, got: {}",
                bad,
                err
            );
        }
    }

    #[test]
    fn the_range_bounds_themselves_are_accepted() {
        for good in [LIST_LIMIT_MIN, LIST_LIMIT_MAX] {
            let parsed = parse_list_args(&args(&["--limit", &good.to_string()]))
                .unwrap_or_else(|e| panic!("limit {} should be accepted, got: {}", good, e));
            assert_eq!(parsed.limit, good);
        }
    }

    #[test]
    fn a_flag_missing_its_value_is_rejected() {
        for flag in ["--limit", "--category"] {
            let err = parse_list_args(&args(&[flag])).unwrap_err();
            assert!(err.contains("expects a value"), "got: {}", err);
        }
    }

    #[test]
    fn an_unknown_flag_is_rejected() {
        let err = parse_list_args(&args(&["--tags", "work"])).unwrap_err();
        assert!(err.contains("unknown flag"), "got: {}", err);
    }

    // ---------------------------------------------------------------------
    // add (#216)
    //
    // The bug these exist for was silent: `args[2..].join(" ")` made every
    // argument content, so a flag became text and the memory was stored in the
    // wrong category with exit code 0 and a success line printed. So these
    // assert the *parsed values*, never merely that parsing succeeded.
    // ---------------------------------------------------------------------

    #[test]
    fn add_without_flags_matches_the_reference_defaults() {
        let parsed = parse_add_args(&args(&["a note"])).unwrap();
        assert_eq!(parsed.content, "a note");
        // `add_p.add_argument("--category", default="general")`.
        assert_eq!(parsed.category, "general");
        assert!(parsed.tags.is_empty());
    }

    #[test]
    fn the_exact_invocation_from_the_issue_no_longer_swallows_its_flag() {
        // Verbatim from #216. Before: content became
        // "written by the rust port --category engineering", category "general".
        let parsed = parse_add_args(&args(&[
            "written by the rust port",
            "--category",
            "engineering",
        ]))
        .unwrap();
        assert_eq!(parsed.content, "written by the rust port");
        assert_eq!(parsed.category, "engineering");
        assert!(
            !parsed.content.contains("--category"),
            "the flag leaked into the content: {:?}",
            parsed.content
        );
    }

    #[test]
    fn add_parses_tags_the_way_the_reference_splits_them() {
        let parsed = parse_add_args(&args(&["note", "--tags", " work , important ,"])).unwrap();
        // `_split_tags`: comma-separated, trimmed, blanks dropped -- so the
        // trailing comma is not an empty tag.
        assert_eq!(parsed.tags, vec!["work", "important"]);
    }

    #[test]
    fn an_empty_tags_value_is_no_tags_rather_than_one_blank_tag() {
        let parsed = parse_add_args(&args(&["note", "--tags", ""])).unwrap();
        assert!(parsed.tags.is_empty());
    }

    #[test]
    fn add_accepts_flags_before_the_content() {
        let parsed = parse_add_args(&args(&["--category", "work", "the note"])).unwrap();
        assert_eq!(parsed.content, "the note");
        assert_eq!(parsed.category, "work");
    }

    #[test]
    fn unquoted_words_still_join_into_one_content() {
        // A deliberate superset of the reference, which declares a single
        // positional and would reject this. Preserved because it is what this
        // CLI already did and what unquoted shell input produces.
        let parsed = parse_add_args(&args(&["several", "unquoted", "words"])).unwrap();
        assert_eq!(parsed.content, "several unquoted words");
    }

    #[test]
    fn a_double_dash_lets_content_start_with_dashes() {
        let parsed = parse_add_args(&args(&["--", "--not-a-flag"])).unwrap();
        assert_eq!(parsed.content, "--not-a-flag");
        assert_eq!(parsed.category, "general");
    }

    #[test]
    fn an_unknown_add_flag_is_rejected_rather_than_stored() {
        // The whole point. `--limit` belongs to search/list, not add; before
        // this it would have been silently appended to the memory's text.
        let err = parse_add_args(&args(&["note", "--limit", "5"])).unwrap_err();
        assert!(err.contains("unknown flag"), "got: {}", err);
    }

    #[test]
    fn add_with_no_content_is_rejected() {
        let err = parse_add_args(&args(&[])).unwrap_err();
        assert!(err.contains("Usage"), "got: {}", err);
        // A lone flag is not content either.
        let err = parse_add_args(&args(&["--category", "work"])).unwrap_err();
        assert!(err.contains("Usage"), "got: {}", err);
    }

    #[test]
    fn an_add_flag_missing_its_value_is_rejected() {
        let err = parse_add_args(&args(&["note", "--category"])).unwrap_err();
        assert!(err.contains("expects a value"), "got: {}", err);
    }

    // ---------------------------------------------------------------------
    // search (#216)
    // ---------------------------------------------------------------------

    #[test]
    fn search_without_flags_matches_the_reference_defaults() {
        let parsed = parse_search_args(&args(&["a query"])).unwrap();
        assert_eq!(parsed.query, "a query");
        // `search_p.add_argument("--limit", type=int, default=20)`.
        assert_eq!(parsed.limit, 20);
        // The reference builds MARKDOWN unless --json, and this CLI's own
        // `list` already defaults the same way.
        assert!(!parsed.as_json);
    }

    #[test]
    fn search_limit_is_honoured_rather_than_absorbed_into_the_query() {
        // Before: `--limit 1` returned the same results as no flag, because
        // "fact --limit 1" was the query and the limit stayed hardcoded at 20.
        let parsed = parse_search_args(&args(&["fact", "--limit", "1"])).unwrap();
        assert_eq!(parsed.query, "fact");
        assert_eq!(parsed.limit, 1);
        assert!(
            !parsed.query.contains("--limit"),
            "the flag leaked into the query: {:?}",
            parsed.query
        );
    }

    #[test]
    fn search_json_opts_in() {
        let parsed = parse_search_args(&args(&["q", "--json"])).unwrap();
        assert!(parsed.as_json);
    }

    #[test]
    fn a_non_numeric_search_limit_is_rejected() {
        let err = parse_search_args(&args(&["q", "--limit", "lots"])).unwrap_err();
        assert!(err.contains("expects a number"), "got: {}", err);
    }

    #[test]
    fn an_out_of_range_search_limit_is_rejected_rather_than_clamped() {
        // Same reasoning as `list`: `search_memories` clamps silently, and a
        // caller who asked for 500 should be told, not handed a short page.
        let err = parse_search_args(&args(&["q", "--limit", "100000"])).unwrap_err();
        assert!(err.contains("must be between"), "got: {}", err);
    }

    #[test]
    fn an_unknown_search_flag_is_rejected() {
        let err = parse_search_args(&args(&["q", "--category", "work"])).unwrap_err();
        assert!(err.contains("unknown flag"), "got: {}", err);
    }

    #[test]
    fn search_with_no_query_is_rejected() {
        let err = parse_search_args(&args(&["--json"])).unwrap_err();
        assert!(err.contains("Usage"), "got: {}", err);
    }

    // ---------------------------------------------------------------------
    // configure
    //
    // These manipulate REMIND_ME_SYNC_SECRET, a process-global, so they are
    // serialised behind one mutex and always restore what they found. Without
    // that they pass alone and fail under the default thread-per-test.
    // ---------------------------------------------------------------------

    use std::sync::Mutex;
    static SECRET_LOCK: Mutex<()> = Mutex::new(());

    /// Run `f` with REMIND_ME_SYNC_SECRET set (or removed, for `None`).
    fn with_secret<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = SECRET_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var(remind_me_core::sync::SYNC_SECRET_ENV).ok();
        match value {
            Some(v) => crate::test_env::set_var(remind_me_core::sync::SYNC_SECRET_ENV, v),
            None => crate::test_env::remove_var(remind_me_core::sync::SYNC_SECRET_ENV),
        }
        let result = f();
        match previous {
            Some(v) => crate::test_env::set_var(remind_me_core::sync::SYNC_SECRET_ENV, v),
            None => crate::test_env::remove_var(remind_me_core::sync::SYNC_SECRET_ENV),
        }
        result
    }

    #[test]
    fn bare_configure_asks_for_no_sync() {
        let parsed = with_secret(None, || {
            parse_configure_args(&args(&[])).expect("bare `configure` is valid")
        });
        assert_eq!(parsed, ConfigureArgs::default());
    }

    #[test]
    fn a_full_sync_triple_parses() {
        let parsed = with_secret(Some("s3cret"), || {
            parse_configure_args(&args(&[
                "--node-id",
                "laptop",
                "--hub-url",
                "http://127.0.0.1:8765",
                "--peer-port",
                "9000",
                "--sync-interval",
                "30",
            ]))
            .unwrap()
        });
        assert_eq!(parsed.node_id.as_deref(), Some("laptop"));
        assert_eq!(parsed.hub_url.as_deref(), Some("http://127.0.0.1:8765"));
        assert_eq!(parsed.peer_port, Some(9000));
        assert_eq!(parsed.sync_interval, Some(30));
    }

    /// The point of the whole check: sync is off unless all three are present,
    /// so a partial triple must be an error rather than a config that looks
    /// written and silently never syncs.
    #[test]
    fn any_partial_sync_triple_is_refused() {
        let cases: [(&[&str], Option<&str>, &str); 3] = [
            (&["--node-id", "laptop"], Some("s3cret"), "--hub-url"),
            (&["--hub-url", "http://h:8765"], Some("s3cret"), "--node-id"),
            (
                &["--node-id", "laptop", "--hub-url", "http://h:8765"],
                None,
                "REMIND_ME_SYNC_SECRET",
            ),
        ];
        for (flags, secret, expected) in cases {
            let err = with_secret(secret, || parse_configure_args(&args(flags)).unwrap_err());
            assert!(
                err.contains(expected) && err.contains("silently disabled"),
                "flags {:?} should name {} as missing, got: {}",
                flags,
                expected,
                err
            );
        }
    }

    /// A secret in the environment on its own still counts as asking for sync
    /// -- otherwise `REMIND_ME_SYNC_SECRET=... configure` would quietly write
    /// an unsynced config, which is the exact trap this guards.
    #[test]
    fn a_lone_secret_in_the_environment_still_demands_the_rest() {
        let err = with_secret(Some("s3cret"), || {
            parse_configure_args(&args(&[])).unwrap_err()
        });
        assert!(
            err.contains("--node-id") && err.contains("--hub-url"),
            "got: {}",
            err
        );
    }

    /// Whitespace is not a secret. Without the trim this passes the presence
    /// check and writes `SYNC_SECRET="   "`, which the hub rejects on every
    /// request -- a working-looking config that never syncs.
    #[test]
    fn a_blank_secret_counts_as_absent() {
        let err = with_secret(Some("   "), || {
            parse_configure_args(&args(&[
                "--node-id",
                "laptop",
                "--hub-url",
                "http://h:8765",
            ]))
            .unwrap_err()
        });
        assert!(err.contains("REMIND_ME_SYNC_SECRET"), "got: {}", err);
    }

    #[test]
    fn a_secret_flag_is_refused_with_the_reason() {
        for flag in ["--secret", "--sync-secret"] {
            let err = with_secret(None, || {
                parse_configure_args(&args(&[flag, "s3cret"])).unwrap_err()
            });
            assert!(
                err.contains("REMIND_ME_SYNC_SECRET") && err.contains("/proc"),
                "{} should explain why it does not exist, got: {}",
                flag,
                err
            );
        }
    }

    #[test]
    fn a_non_numeric_peer_port_is_rejected() {
        let err = with_secret(None, || {
            parse_configure_args(&args(&["--peer-port", "http"])).unwrap_err()
        });
        assert!(err.contains("--peer-port expects a number"), "got: {}", err);
    }

    #[test]
    fn a_configure_flag_missing_its_value_is_rejected() {
        for flag in [
            "--node-id",
            "--hub-url",
            "--peer-port",
            "--sync-interval",
            "--db-path",
        ] {
            let err = with_secret(None, || parse_configure_args(&args(&[flag])).unwrap_err());
            assert!(err.contains("expects a value"), "{}: {}", flag, err);
        }
    }

    #[test]
    fn an_unknown_configure_flag_is_rejected() {
        let err = with_secret(None, || {
            parse_configure_args(&args(&["--hub", "http://h"])).unwrap_err()
        });
        assert!(err.contains("unknown flag"), "got: {}", err);
    }
}
