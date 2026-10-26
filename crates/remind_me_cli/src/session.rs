//! `session start|end|timeline`, `capture-transcript`, `context` and `brief`:
//! the subcommands a client's hooks call; and `import-sessions`, the one-time
//! backfill of the transcripts captured before the hooks were installed. The flags and the JSON they print are
//! the contract the hook scripts are written against; the work is
//! `remind_me_core::session_ops`, run wherever the store is (the daemon or
//! this process).
//!
//! Environment fallbacks, so a hook can export once and call many times:
//! `REMIND_ME_SESSION_ID` for `--session-id`, `REMIND_ME_CWD` for `--cwd`
//! (else the current directory).

use crate::daemon::Store;
use remind_me_core::daemon::ops::Op;
use serde_json::Value;
use std::path::{Path, PathBuf};

const USAGE: &str = "\
Usage:
  rusty-remind-me session start --session-id ID [--cwd DIR] [--client NAME]
  rusty-remind-me session end --session-id ID [--cwd DIR] [--reason R]
  rusty-remind-me session timeline --session-id ID [--json]
  rusty-remind-me capture-transcript PATH.jsonl --session-id ID [--cwd DIR]
                  [--reason stop|precompact|end] [--json]
  rusty-remind-me import-sessions DIR   (a Claude Code `projects` folder)
  rusty-remind-me context [--cwd DIR] [--project P] [--branch B] [--prompt TEXT]
                  [--budget TOKENS] [--hits-only] [--json]
  rusty-remind-me brief PROJECT [--since 7d|12h|RFC3339] [--budget TOKENS] [--json]

--session-id defaults to $REMIND_ME_SESSION_ID and --cwd to $REMIND_ME_CWD,
then the current directory. Every subcommand prints one JSON object, except
`session timeline`, `context` and `brief` without --json, which print Markdown.";

/// Default `--budget` for `context`, in tokens.
const DEFAULT_BUDGET: usize = 2000;

/// Flags common to the subcommands; each takes only the ones it knows.
#[derive(Debug, Default, PartialEq, Eq)]
struct Flags {
    positional: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    client: Option<String>,
    reason: Option<String>,
    project: Option<String>,
    branch: Option<String>,
    prompt: Option<String>,
    since: Option<String>,
    budget: Option<usize>,
    json: bool,
    hits_only: bool,
}

fn parse_flags(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--json" || arg == "--hits-only" {
            if arg == "--json" {
                flags.json = true;
            } else {
                flags.hits_only = true;
            }
            i += 1;
            continue;
        }
        if !arg.starts_with("--") {
            if flags.positional.replace(arg.to_string()).is_some() {
                return Err(format!("Error: unexpected argument {arg:?}.\n{USAGE}"));
            }
            i += 1;
            continue;
        }
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("Error: {arg} expects a value.\n{USAGE}"))?
            .clone();
        match arg {
            "--session-id" => flags.session_id = Some(value),
            "--cwd" => flags.cwd = Some(value),
            "--client" => flags.client = Some(value),
            "--reason" => flags.reason = Some(value),
            "--project" => flags.project = Some(value),
            "--branch" => flags.branch = Some(value),
            "--prompt" => flags.prompt = Some(value),
            "--since" => flags.since = Some(value),
            "--budget" => {
                flags.budget = Some(value.parse().map_err(|_| {
                    format!("Error: --budget expects a number of tokens, got {value:?}.")
                })?)
            }
            other => return Err(format!("Error: unknown flag {other}.\n{USAGE}")),
        }
        i += 2;
    }
    Ok(flags)
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// An absolute path for `raw`, resolved against the current directory: the
/// daemon's working directory is not ours.
fn absolute(raw: &str) -> Result<PathBuf, String> {
    std::path::absolute(Path::new(raw)).map_err(|e| format!("Error: bad path {raw:?}: {e}"))
}

impl Flags {
    fn session_id(&self) -> Result<String, String> {
        self.session_id
            .clone()
            .or_else(|| env_nonempty("REMIND_ME_SESSION_ID"))
            .ok_or_else(|| {
                format!("Error: --session-id (or REMIND_ME_SESSION_ID) is required.\n{USAGE}")
            })
    }

    fn cwd(&self) -> Result<PathBuf, String> {
        let raw = self
            .cwd
            .clone()
            .or_else(|| env_nonempty("REMIND_ME_CWD"))
            .map(Ok)
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|d| d.display().to_string())
                    .map_err(|e| format!("Error: no current directory: {e}"))
            })?;
        absolute(&raw)
    }
}

/// The `Op` a subcommand runs, and whether it prints Markdown from the
/// reply's `markdown`/`context` member rather than JSON.
enum Plan {
    Json(Op),
    /// `session timeline`: `{"markdown", ..}` or null.
    Timeline(Op, bool),
    /// `context` and `brief`: `{"brief", "context"}`.
    Context(Op, bool),
}

fn plan(command: &str, rest: &[String]) -> Result<Plan, String> {
    match command {
        "session" => plan_session(rest),
        "capture-transcript" => {
            let flags = parse_flags(rest)?;
            let path = flags.positional.clone().ok_or_else(|| {
                format!("Error: capture-transcript expects a transcript path.\n{USAGE}")
            })?;
            Ok(Plan::Json(Op::CaptureTranscript {
                path: absolute(&path)?,
                session_id: flags.session_id()?,
                cwd: Some(flags.cwd()?),
                reason: flags.reason,
            }))
        }
        "import-sessions" => {
            let flags = parse_flags(rest)?;
            let dir = flags
                .positional
                .ok_or_else(|| format!("Error: import-sessions expects a directory.\n{USAGE}"))?;
            Ok(Plan::Json(Op::ImportSessions {
                dir: absolute(&dir)?,
            }))
        }
        "context" => {
            let flags = parse_flags(rest)?;
            Ok(Plan::Context(
                Op::Context {
                    cwd: Some(flags.cwd()?),
                    project: flags.project.clone(),
                    branch: flags.branch.clone(),
                    prompt: flags.prompt.clone(),
                    budget: flags.budget.unwrap_or(DEFAULT_BUDGET),
                    hits_only: flags.hits_only,
                },
                flags.json,
            ))
        }
        "brief" => {
            let flags = parse_flags(rest)?;
            let project = flags
                .positional
                .clone()
                .ok_or_else(|| format!("Error: brief expects a project name.\n{USAGE}"))?;
            Ok(Plan::Context(
                Op::Brief {
                    project,
                    since: flags.since.clone(),
                    budget: flags
                        .budget
                        .unwrap_or(remind_me_core::context_brief::DEFAULT_TOKEN_BUDGET),
                },
                flags.json,
            ))
        }
        other => Err(format!("Error: unknown command {other:?}.\n{USAGE}")),
    }
}

fn plan_session(rest: &[String]) -> Result<Plan, String> {
    let Some((action, rest)) = rest.split_first() else {
        return Err(format!(
            "Error: session expects start, end or timeline.\n{USAGE}"
        ));
    };
    let flags = parse_flags(rest)?;
    match action.as_str() {
        "start" => Ok(Plan::Json(Op::SessionStart {
            session_id: flags.session_id()?,
            cwd: flags.cwd()?,
            client: flags.client,
        })),
        "end" => Ok(Plan::Json(Op::SessionEnd {
            session_id: flags.session_id()?,
            cwd: flags.cwd()?,
            reason: flags.reason,
        })),
        "timeline" => Ok(Plan::Timeline(
            Op::SessionTimeline {
                session_id: flags.session_id()?,
            },
            flags.json,
        )),
        other => Err(format!("Error: unknown session action {other:?}.\n{USAGE}")),
    }
}

/// What to print for `reply`: JSON, or the Markdown member when asked.
fn render(plan: &Plan, reply: Value) -> String {
    let pretty = |v: &Value| serde_json::to_string(v).unwrap_or_default();
    match plan {
        Plan::Json(_) => pretty(&reply),
        Plan::Timeline(_, true) => pretty(&reply),
        // The brief's JSON alone: the Markdown beside it would double the
        // output past the budget.
        Plan::Context(_, true) => pretty(reply.get("brief").unwrap_or(&Value::Null)),
        Plan::Timeline(_, false) => match reply.get("markdown").and_then(Value::as_str) {
            Some(text) => text.to_string(),
            None => "No session found.".to_string(),
        },
        Plan::Context(_, false) => reply
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

fn op_of(plan: Plan) -> (Op, Plan) {
    match &plan {
        Plan::Json(op) | Plan::Timeline(op, _) | Plan::Context(op, _) => (op.clone(), plan),
    }
}

/// Run `command` (`session`, `capture-transcript`, `import-sessions`, `context` or `brief`) with its
/// arguments, printing the result. Exits 1 with a message on bad arguments.
pub fn command(
    command: &str,
    rest: &[String],
    db_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let planned = match plan(command, rest) {
        Ok(planned) => planned,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };
    let (op, planned) = op_of(planned);
    let reply: Value = Store::open(db_path)?.call(op)?;
    println!("{}", render(&planned, reply));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn start_takes_session_cwd_and_client() {
        let planned = plan(
            "session",
            &args(&[
                "start",
                "--session-id",
                "s1",
                "--cwd",
                "/work",
                "--client",
                "claude-code",
            ]),
        )
        .unwrap();
        match planned {
            Plan::Json(Op::SessionStart {
                session_id,
                cwd,
                client,
            }) => {
                assert_eq!(session_id, "s1");
                assert_eq!(cwd, std::path::absolute("/work").unwrap());
                assert_eq!(client.as_deref(), Some("claude-code"));
            }
            _ => panic!("expected SessionStart"),
        }
    }

    #[test]
    fn capture_transcript_takes_a_path_and_reason() {
        let planned = plan(
            "capture-transcript",
            &args(&[
                "/t/x.jsonl",
                "--session-id",
                "s1",
                "--cwd",
                "/w",
                "--reason",
                "precompact",
                "--json",
            ]),
        )
        .unwrap();
        match planned {
            Plan::Json(Op::CaptureTranscript {
                path,
                session_id,
                cwd,
                reason,
            }) => {
                assert_eq!(path, std::path::absolute("/t/x.jsonl").unwrap());
                assert_eq!(session_id, "s1");
                assert_eq!(cwd, Some(std::path::absolute("/w").unwrap()));
                assert_eq!(reason.as_deref(), Some("precompact"));
            }
            _ => panic!("expected CaptureTranscript"),
        }
    }

    #[test]
    fn import_sessions_takes_one_directory() {
        match plan("import-sessions", &args(&["/h/.claude/projects"])).unwrap() {
            Plan::Json(Op::ImportSessions { dir }) => {
                assert_eq!(dir, std::path::absolute("/h/.claude/projects").unwrap());
            }
            _ => panic!("expected ImportSessions"),
        }
        assert!(plan("import-sessions", &args(&[])).is_err());
        assert!(plan("import-sessions", &args(&["a", "b"])).is_err());
    }

    #[test]
    fn context_defaults_its_budget_and_reads_every_filter() {
        let planned = plan(
            "context",
            &args(&[
                "--cwd",
                "/w",
                "--project",
                "p",
                "--branch",
                "b",
                "--prompt",
                "why",
                "--json",
            ]),
        )
        .unwrap();
        match planned {
            Plan::Context(
                Op::Context {
                    project,
                    branch,
                    prompt,
                    budget,
                    ..
                },
                true,
            ) => {
                assert_eq!(project.as_deref(), Some("p"));
                assert_eq!(branch.as_deref(), Some("b"));
                assert_eq!(prompt.as_deref(), Some("why"));
                assert_eq!(budget, DEFAULT_BUDGET);
            }
            _ => panic!("expected Context"),
        }
    }

    #[test]
    fn brief_takes_a_project_window_and_budget() {
        match plan("brief", &args(&["mux", "--since", "2d", "--budget", "900"])).unwrap() {
            Plan::Context(
                Op::Brief {
                    project,
                    since,
                    budget,
                },
                false,
            ) => {
                assert_eq!(project, "mux");
                assert_eq!(since.as_deref(), Some("2d"));
                assert_eq!(budget, 900);
            }
            _ => panic!("expected Brief"),
        }
        match plan("brief", &args(&["mux", "--json"])).unwrap() {
            Plan::Context(Op::Brief { since, budget, .. }, true) => {
                assert_eq!(since, None);
                assert_eq!(budget, remind_me_core::context_brief::DEFAULT_TOKEN_BUDGET);
            }
            _ => panic!("expected Brief"),
        }
    }

    #[test]
    fn bad_arguments_are_refused_with_the_usage() {
        assert!(plan("session", &args(&[])).is_err());
        assert!(plan("session", &args(&["pause"])).is_err());
        assert!(plan("session", &args(&["start", "--bogus", "x"])).is_err());
        assert!(plan("session", &args(&["start", "--cwd"])).is_err());
        assert!(plan("capture-transcript", &args(&["--session-id", "s"])).is_err());
        assert!(plan(
            "capture-transcript",
            &args(&["a", "b", "--session-id", "s"])
        )
        .is_err());
        assert!(plan("context", &args(&["--budget", "lots"])).is_err());
        assert!(plan("brief", &args(&[])).is_err());
        assert!(plan("brief", &args(&["a", "b"])).is_err());
    }

    #[test]
    fn markdown_is_unwrapped_unless_json_is_asked_for() {
        let reply = serde_json::json!({"brief": {"sections": []}, "context": "# Hi"});
        let op = Op::Context {
            cwd: None,
            project: None,
            branch: None,
            prompt: None,
            budget: 1,
            hits_only: false,
        };
        assert_eq!(
            render(&Plan::Context(op.clone(), false), reply.clone()),
            "# Hi"
        );
        let json = render(&Plan::Context(op, true), reply);
        assert!(
            json.contains("\"sections\"") && !json.contains("# Hi"),
            "{json}"
        );
    }
}
