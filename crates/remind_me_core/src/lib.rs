// Lets `test_env.rs` name this crate `remind_me_core`, as it does from
// every other test binary it is `#[path]`-included into.
#[cfg(test)]
extern crate self as remind_me_core;
#[cfg(test)]
mod test_env;

pub mod analytics;
pub mod ann_index;
pub mod api_keys;
pub mod archive;
pub mod attachments;
pub mod audio_import;
pub mod backup;
pub mod boundary;
pub mod capture;
pub mod cloud_backup;
pub mod code_refs;
pub mod compaction;
pub mod consolidation;
pub mod context;
pub mod context_brief;
pub mod contradictions;
pub mod daemon;
pub mod db;
pub mod dbs_import;
pub mod digest;
pub mod embedder;
pub mod entity;
pub mod episodes;
pub mod events;
pub mod expansion;
pub mod export;
pub mod extract;
pub mod fts;
pub mod history;
pub mod ics;
pub mod image_import;
pub mod import_paths;
pub mod kinds;
pub mod importer;
pub mod maintenance;
pub mod mempalace_import;
pub mod metrics;
pub mod models;
pub mod normalize;
pub mod notifications;
pub mod obsidian_import;
pub mod pdf_import;
pub mod pid;
pub mod promotion;
pub mod query_expansion;
pub mod rate_limit;
pub mod readwise_import;
pub mod recalibrate;
pub mod redact;
pub mod reminders;
pub mod resolve;
pub mod remote;
pub mod reranker;
pub mod retrieval;
pub mod saved_searches;
pub mod scheduler;
pub mod session_ops;
pub mod sidecars;
pub mod skeleton;
pub mod stats;
pub mod status;
pub mod sync;
pub mod telemetry;
#[doc(hidden)]
pub mod testing;
pub mod tool_profiles;
pub mod transcript;
pub mod undo_import;
pub mod updater;
pub mod vectors;
pub mod vitality;
pub mod watchdog;
pub mod watcher;
pub mod webhook;
pub mod wiki;
pub mod wiki_fs;
pub mod wiki_import;
pub mod worklog;

pub use db::Database;
pub use entity::*;
pub use models::*;
pub use wiki::*;
// Deliberately not glob-re-exported: `wiki_import::slugify` would collide at
// the crate root with anything `wiki::*` grows later. Use `wiki_import::…`.
