use super::{flag, json_flag, option, required};
use clap::{ArgGroup, Command};

pub(super) fn command() -> Command {
    Command::new("task")
        .about("Manage durable endpoint-local task reports (no agent input is sent)")
        .subcommand_required(true)
        .after_long_help("Examples:\n  herdr task create --title 'Fix parser' --nonce issue-42-create --json\n  herdr task list --json\n  herdr task get task_ID --json\n  herdr task source --pane \"$HERDR_PANE_ID\" --json")
        .subcommand(Command::new("adapter")
            .about("Bound one-shot JSON Tasks tools, not MCP; requires exact Herdr launch environment")
            .subcommand_required(true)
            .after_help("Examples:\n  \"$HERDR_BIN_PATH\" task adapter discover\n  printf '%s' '{\"scope\":\"current\",\"limit\":10}' | \"$HERDR_BIN_PATH\" task adapter call list\nRequires HERDR_SOCKET_PATH, HERDR_BIN_PATH, HERDR_PANE_ID and HERDR_WORKSPACE_ID. No default endpoint or PATH fallback. Reporting only; never automatically execute todos.")
            .subcommand(Command::new("discover")
                .about("Bound read-only source/list probes; never mutates Tasks")
                .after_help("Examples:\n  \"$HERDR_BIN_PATH\" task adapter discover"))
            .subcommand(Command::new("call")
                .about("Read one JSON object from stdin (16 KiB, 3 second total deadline); no automatic retries")
                .arg(required("method", "METHOD").value_parser(["list", "get", "create", "update", "append_conclusion", "source_context"]))
                .after_help("Examples:\n  printf '%s' '{\"scope\":\"all\"}' | \"$HERDR_BIN_PATH\" task adapter call list\n  \"$HERDR_BIN_PATH\" task adapter discover > task-context.json\n  python3 -c 'import json; c=json.load(open(\"task-context.json\")); print(json.dumps({\"expected_context\":c[\"context_fingerprint\"],\"title\":\"Fix parser\",\"nonce\":\"issue-42-create\"}))' > task-create.json\n  \"$HERDR_BIN_PATH\" task adapter call create < task-create.json\nEvery mutation requires caller-retained expected_context from discovery/source_context. Capture it only for new work; retain it with the original nonce, payload and revision on uncertainty. stale_context: this attempt was not dispatched, previous outcome unknown. Never refresh/rebase uncertain old work or generate a new nonce automatically. CAS conflicts require caller review; never blindly overwrite. Unknown/reserved fields rejected. Preflight is not an atomic boot/store fence.")))
        .subcommand(provenance(text_input(Command::new("create")
            .about("Create a durable task; retry with the same nonce and identical payload")
            .arg(option("title", "TEXT").required(true))
            .arg(option("nonce", "KEY").required(true))
            .arg(status())
            .arg(option("workspace", "ID"))
            .arg(option("workspace-label", "TEXT"))
            .arg(json_flag())
            .after_help("Examples:\n  herdr task create --title 'Fix parser' --nonce issue-42-create --json\n  cat plan.txt | herdr task create --title 'Fix parser' --nonce issue-42-create --description-stdin"), "description", false)))
        .subcommand(Command::new("source")
            .about("Observe current endpoint store/boot and an existing source pane (not authorship proof)")
            .arg(option("pane", "PANE_ID").required(true))
            .arg(json_flag())
            .after_help("Examples:\n  herdr task source --pane \"$HERDR_PANE_ID\" --json\nUse result.context.boot_id and pane_id as explicit --source-boot/--source-pane on create or add-conclusion; do not reuse after restart."))
        .subcommand(Command::new("list")
            .about("List task summaries; use next_after as --after for the next page")
            .arg(option("workspace", "ID"))
            .arg(option("status", "STATUS"))
            .arg(option("after", "TASK_ID"))
            .arg(option("limit", "N").value_parser(clap::value_parser!(usize)))
            .arg(json_flag())
            .after_help("Examples:\n  herdr task list --json\n  herdr task list --workspace ws_1 --limit 10 --after task_ID --json"))
        .subcommand(Command::new("get")
            .about("Read a task, current revision and saved conclusions")
            .arg(required("task_id", "TASK_ID"))
            .arg(json_flag())
            .after_help("Examples:\n  herdr task get task_ID --json"))
        .subcommand(text_input(Command::new("update")
            .about("Explicit CAS update; done and reported verification are independent")
            .arg(required("task_id", "TASK_ID"))
            .arg(revision())
            .arg(option("title", "TEXT"))
            .arg(status())
            .arg(option("reported-verification", "RESULT").value_parser(["unknown", "pass", "fail"]))
            .arg(option("verification-evidence", "TEXT"))
            .arg(option("workspace", "ID"))
            .arg(option("workspace-label", "TEXT"))
            .arg(json_flag())
            .group(ArgGroup::new("change").args(["title", "status", "reported-verification", "verification-evidence", "workspace", "workspace-label", "description", "description-file", "description-stdin"]).required(true).multiple(true))
            .after_help("Examples:\n  herdr task update task_ID --expected-revision 1 --status working --json\n  herdr task update task_ID --expected-revision 2 --status done --reported-verification pass --verification-evidence 'cargo test: 12 passed' --json"), "description", false))
        .subcommand(provenance(text_input(Command::new("add-conclusion")
            .about("Append an immutable conclusion; preserve nonce and payload for retries")
            .arg(required("task_id", "TASK_ID"))
            .arg(revision())
            .arg(option("nonce", "KEY").required(true))
            .arg(json_flag())
            .after_help("Examples:\n  herdr task add-conclusion task_ID --expected-revision 3 --nonce issue-42-conclusion --text 'Fixed and checked' --json\n  cat result.txt | herdr task add-conclusion task_ID --expected-revision 3 --nonce issue-42-conclusion --text-stdin"), "text", true)))
}
fn status() -> clap::Arg {
    option("status", "STATUS").value_parser(["todo", "working", "blocked", "review", "done"])
}
fn revision() -> clap::Arg {
    option("expected-revision", "REVISION")
        .required(true)
        .value_parser(clap::value_parser!(u64))
}
fn provenance(command: Command) -> Command {
    command
        .arg(option("reporter", "LABEL"))
        .arg(option("agent", "LABEL"))
        .arg(option("source-boot", "BOOT_ID"))
        .arg(option("source-pane", "PANE_ID").requires("source-boot"))
        .arg(option("agent-session", "REFERENCE"))
}
fn text_input(command: Command, name: &str, required: bool) -> Command {
    // Clap IDs are static because these are the only two supported text inputs.
    let (file, stdin, group) = if name == "text" {
        ("text-file", "text-stdin", "text-input")
    } else {
        ("description-file", "description-stdin", "description-input")
    };
    let name = if name == "text" {
        "text"
    } else {
        "description"
    };
    command
        .arg(option(name, "TEXT"))
        .arg(option(file, "PATH"))
        .arg(flag(stdin).help("Read bounded UTF-8 text from stdin; never prompts"))
        .group(
            ArgGroup::new(group)
                .args([name, file, stdin])
                .required(required),
        )
}
