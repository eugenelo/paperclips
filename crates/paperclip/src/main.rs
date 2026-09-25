// paperclips — the clips side of the agent friction log.
// Companion to papercuts: papercuts logs friction, paperclip logs wins.

use clap::Parser;
use paper_core::error::AppError;
use paper_core::Impact;

mod review;
mod commands;

#[derive(Debug, Parser)]
#[command(
    name = "paperclip",
    version,
    about = "Log wins and clips — the positive side of the agent friction log.",
    long_about = None,
    arg_required_else_help = true,
    subcommand_required = true,
    color = clap::ColorChoice::Never,
    rename_all = "kebab-case"
)]
pub struct Cli {
    #[arg(long, global = true, value_name = "PATH")]
    pub file: Option<std::path::PathBuf>,

    #[arg(long, global = true)]
    pub pretty: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    Add(AddArgs),
    List(ListArgs),
    Promote(PromoteArgs),
    Note(NoteArgs),
    Top(TopArgs),
    Review(ReviewArgs),
    Schema,
    Doctor,
}

#[derive(Debug, clap::Args)]
pub struct AddArgs {
    #[arg(value_name = "TEXT")]
    pub what: Option<String>,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    #[arg(long, value_enum, default_value_t = Impact::Nice)]
    pub impact: Impact,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub force: bool,
    #[arg(long = "where", value_name = "COMPONENT")]
    pub where_loc: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    #[arg(long)]
    pub tag: Option<String>,
    #[arg(long, value_enum)]
    pub impact: Option<Impact>,
    #[arg(long, value_enum, default_value_t = StatusFilter::Open)]
    pub status: StatusFilter,
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    pub format: OutputFormat,
    #[arg(long = "where", value_name = "COMPONENT")]
    pub where_loc: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct PromoteArgs {
    #[arg(value_name = "ID")]
    pub id: String,
    #[arg(long)]
    pub agent: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct NoteArgs {
    #[arg(value_name = "ID")]
    pub id: String,
    #[arg(value_name = "TEXT")]
    pub text: String,
    #[arg(long)]
    pub agent: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct TopArgs {
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
}

#[derive(Debug, clap::Args)]
pub struct ReviewArgs {
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    pub format: OutputFormat,
}


#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum StatusFilter {
    Open,
    Promoted,
    Noted,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Json,
    Md,
}

/// Dispatch CLI commands, including attributed note and promote events.
/// @return Exits with the command's contract status.
fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => match error.kind() {
            clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                let _ = error.print();
                std::process::exit(0);
            }
            _ => {
                let app_error = AppError::invalid_argument(
                    error.to_string(),
                    "Run `paperclip --help` or `paperclip schema` for accepted commands and values.",
                );
                std::process::exit(paper_core::output::write_error(&app_error));
            }
        },
    };
    let code = match cli.command {
        Command::Review(args) => review::run(args.format, None, cli.file, cli.pretty)
            .unwrap_or_else(|error| paper_core::output::write_error(&error)),
        Command::Add(args) => commands::add(
            args.what, args.agent, args.tags, args.impact, args.dry_run,
            args.force, args.where_loc, cli.file, cli.pretty,
            paper_core::effective_now().unwrap(),
        ).unwrap_or_else(|error| paper_core::output::write_error(&error)),
        Command::List(args) => commands::list(
            args.tag, args.impact, args.status, args.limit, args.format,
            args.where_loc, cli.file, cli.pretty,
        ).unwrap_or_else(|error| paper_core::output::write_error(&error)),
        Command::Promote(args) => commands::promote(
            args.id, args.agent, cli.file, cli.pretty,
            paper_core::effective_now().unwrap(),
        ).unwrap_or_else(|error| paper_core::output::write_error(&error)),
        Command::Note(args) => commands::note(
            args.id, args.text, args.agent, cli.file, cli.pretty,
            paper_core::effective_now().unwrap(),
        ).unwrap_or_else(|error| paper_core::output::write_error(&error)),
        Command::Top(args) => commands::top(
            args.limit, cli.file, cli.pretty,
        ).unwrap_or_else(|error| paper_core::output::write_error(&error)),
        Command::Doctor => commands::doctor(cli.file, cli.pretty)
            .unwrap_or_else(|error| paper_core::output::write_error(&error)),
        Command::Schema => {
            let schema = serde_json::json!({
                "contract": 1,
                "name": "paperclip",
                "description": "Log wins and clips — the positive side of the agent friction log.",
                "commands": {
                    "add": {"flags": {"--impact": "nice|solid|huge; default nice", "--tag": "TAG; repeatable", "--agent": "NAME", "--dry-run": "boolean", "--where": "COMPONENT"}, "positional": "TEXT or -"},
                    "list": {"flags": {"--tag": "TAG", "--impact": "nice|solid|huge", "--status": "open|promoted|noted|all; default open", "--limit": "N; default 50", "--format": "json|md; default json", "--where": "COMPONENT"}},
                    "promote": {"flags": {"--agent": "NAME"}, "positional": "ID"},
                    "note": {"flags": {"--agent": "NAME"}, "positional": "ID TEXT"},
                    "top": {"flags": {"--limit": "N; default 10"}},
                    "review": {"flags": {"--format": "json|md; default json"}, "description": "Reads both .papercuts.jsonl and .paperclips.jsonl. Emits Fix/Keep/Overlaps digest."},
                    "schema": {"description": "Print this contract."},
                    "doctor": {"description": "Self-check."}
                },
                "record": {
                    "kind": "clip",
                    "id": "cl_<12 lowercase hex>",
                    "what": "string",
                    "tag": "string",
                    "impact": "nice|solid|huge",
                    "status": "open|promoted|noted",
                    "ts": "RFC3339 UTC milliseconds",
                    "agent": "string",
                    "notes": ["string"]
                }
            });
            let meta = paper_core::output::Meta::new();
            if cli.pretty {
                let _ = paper_core::output::write_success(schema, true, meta);
            } else {
                let _ = paper_core::output::write_success(schema, false, meta);
            }
            0
        }

    };
    std::process::exit(code);
}
