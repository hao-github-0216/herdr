from __future__ import annotations

import re
import unittest
from pathlib import Path


PROJECT_ROOT = Path(__file__).resolve().parent.parent
HOT_PATH_SOURCES = (
    PROJECT_ROOT / "src" / "ui.rs",
    *sorted((PROJECT_ROOT / "src" / "ui").rglob("*.rs")),
    PROJECT_ROOT / "src" / "server" / "render_stream.rs",
)
APP_SERVER_SOURCES = (
    *sorted((PROJECT_ROOT / "src" / "app").rglob("*.rs")),
    *sorted((PROJECT_ROOT / "src" / "server").rglob("*.rs")),
)
TEST_MODULE = re.compile(r"(?m)^#\[cfg\(test\)\]\s*\nmod\s+\w+\s*\{")
INPUT_STATE_CALL = re.compile(r"(?:\.|::)input_state\b")
KEYBOARD_STATE_ANSI_CALL = re.compile(
    r"(?:\.|::)(?:keyboard_state_ansi|kitty_keyboard_state_ansi)\b"
)
AGGREGATE_STATE_CALLS = (
    (INPUT_STATE_CALL, "aggregate terminal input state; add a narrow accessor"),
    (KEYBOARD_STATE_ANSI_CALL, "formatted keyboard state"),
)
FORBIDDEN_CALLS = (
    *AGGREGATE_STATE_CALLS,
    (
        re.compile(r"(?:\.|::)screen_text_snapshot\b"),
        "formatted terminal screen snapshot",
    ),
    (
        re.compile(r"\bforeground_job\s*\("),
        "process-tree inspection",
    ),
)


def blank_non_newlines(chars: list[str], start: int, end: int) -> None:
    for index in range(start, end):
        if chars[index] != "\n":
            chars[index] = " "


def mask_comments_and_literals(source: str) -> str:
    chars = list(source)
    index = 0
    while index < len(source):
        if source.startswith("//", index):
            end = source.find("\n", index + 2)
            end = len(source) if end == -1 else end
            blank_non_newlines(chars, index, end)
            index = end
            continue

        if source.startswith("/*", index):
            depth = 1
            end = index + 2
            while end < len(source) and depth > 0:
                if source.startswith("/*", end):
                    depth += 1
                    end += 2
                elif source.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            blank_non_newlines(chars, index, end)
            index = end
            continue

        if source[index] == "r":
            quote = index + 1
            while quote < len(source) and source[quote] == "#":
                quote += 1
            if quote < len(source) and source[quote] == '"':
                suffix = '"' + "#" * (quote - index - 1)
                end = source.find(suffix, quote + 1)
                end = len(source) if end == -1 else end + len(suffix)
                blank_non_newlines(chars, index, end)
                index = end
                continue

        if source[index] == '"':
            end = index + 1
            while end < len(source):
                if source[end] == "\\":
                    end += 2
                elif source[end] == '"':
                    end += 1
                    break
                else:
                    end += 1
            blank_non_newlines(chars, index, min(end, len(source)))
            index = end
            continue

        if source[index] == "'":
            end = index + 2
            if index + 1 < len(source) and source[index + 1] == "\\":
                end += 1
            if end < len(source) and source[end] == "'":
                end += 1
                blank_non_newlines(chars, index, end)
                index = end
                continue

        index += 1

    return "".join(chars)


def production_code(source: str) -> str:
    code = mask_comments_and_literals(source)
    chars = list(code)
    search_from = 0

    while test_module := TEST_MODULE.search(code, search_from):
        depth = 0
        end = test_module.end() - 1
        while end < len(code):
            if code[end] == "{":
                depth += 1
            elif code[end] == "}":
                depth -= 1
                if depth == 0:
                    end += 1
                    break
            end += 1
        blank_non_newlines(chars, test_module.start(), end)
        code = "".join(chars)
        search_from = end

    return code


def find_violations(paths, rules) -> list[str]:
    violations: list[str] = []
    for path in paths:
        code = production_code(path.read_text(encoding="utf-8"))
        for pattern, description in rules:
            for match in pattern.finditer(code):
                line = code.count("\n", 0, match.start()) + 1
                relative_path = path.relative_to(PROJECT_ROOT)
                violations.append(f"{relative_path}:{line}: {description}")
    return violations


class UiHotPathArchitectureTests(unittest.TestCase):
    def test_render_hot_paths_avoid_known_expensive_runtime_queries(self) -> None:
        violations = find_violations(HOT_PATH_SOURCES, FORBIDDEN_CALLS)

        self.assertEqual(
            violations,
            [],
            "Render/layout code must not perform pane-scaled expensive reads:\n"
            + "\n".join(violations),
        )

    def test_app_and_server_avoid_aggregate_terminal_state(self) -> None:
        self.assertTrue(APP_SERVER_SOURCES, "No app/server Rust sources were discovered")
        violations = find_violations(APP_SERVER_SOURCES, AGGREGATE_STATE_CALLS)

        self.assertEqual(
            violations,
            [],
            "App/server code must use narrow terminal-state accessors:\n"
            + "\n".join(violations),
        )

    def test_inspector_client_never_reads_files_or_spawns_processes(self) -> None:
        inspector = PROJECT_ROOT / "src/client/shell/inspector.rs"
        rules = (
            (re.compile(r"\b(?:std|tokio)::(?:fs|process)\b"), "client filesystem/process access"),
            (re.compile(r"\b(?:read_dir|canonicalize|read_confined_file|list_confined_directory)\s*\("), "client path access"),
        )
        self.assertEqual(find_violations((inspector,), rules), [])
        source = inspector.read_text(encoding="utf-8")
        self.assertIn("push_endpoint_method_with_kind", source)
        self.assertIn("inspector_target_current", source)
        self.assertIn("active_snapshot_generation", source)
        self.assertIn("methods.as_ref()", source)

    def test_inspector_reads_are_deferred_before_pane_scaled_api_capture(self) -> None:
        source = (PROJECT_ROOT / "src/server/headless.rs").read_text(encoding="utf-8")
        dispatch = source.index("self.app.start_api_read(msg.request, msg.respond_to)")
        capture = source.index("// Capture toast and effective pane states before the API call")
        self.assertLess(dispatch, capture)
        worker = (PROJECT_ROOT / "src/app/api/reads.rs").read_text(encoding="utf-8")
        self.assertLess(worker.index("try_acquire_owned()"), worker.index(".spawn(move ||"))
        process = (PROJECT_ROOT / "src/app/api/read_process.rs").read_text(encoding="utf-8")
        self.assertIn("tokio::time::timeout(timeout, read)", process)
        self.assertIn("child.kill().await", process)
        self.assertIn("child.wait().await", process)
        for flag in ("--no-pager", "--literal-pathspecs", "--no-ext-diff", "--no-textconv", "core.fsmonitor=false"):
            self.assertIn(flag, worker)
        self.assertIn("filter_driver_overrides", worker)
        self.assertIn("keys.truncated", worker)

    def test_inspector_does_not_extend_frozen_wire_or_terminal_key_dispatch(self) -> None:
        wire = (PROJECT_ROOT / "src/protocol/wire.rs").read_text(encoding="utf-8")
        self.assertNotIn("Inspector", wire)
        self.assertNotIn("FilePathParams", wire)
        input_source = (PROJECT_ROOT / "src/client/shell/input.rs").read_text(encoding="utf-8")
        self.assertNotIn("inspector", input_source)
        inspector = (PROJECT_ROOT / "src/client/shell/inspector.rs").read_text(encoding="utf-8")
        tick = inspector[inspector.index("pub(crate) fn tick_inspector"):]
        self.assertLess(tick.index("if !self.inspector_visible()"), tick.index("queue.pop_front()"))
        self.assertIn("outcome.resize = true", inspector)

    def test_task_transactions_are_background_only_and_outside_pane_scaled_capture(self) -> None:
        source = (PROJECT_ROOT / "src/server/headless.rs").read_text(encoding="utf-8")
        dispatch = source.index("self.app.task_service.start(msg.request, msg.respond_to)")
        self.assertLess(dispatch, source.index("let frozen_alt_screen_read ="))
        self.assertLess(dispatch, source.index("// Capture toast and effective pane states before the API call"))
        service = (PROJECT_ROOT / "src/task_service.rs").read_text(encoding="utf-8")
        self.assertLess(service.index("self.admit()"), service.index(".spawn(move ||"))
        self.assertLess(service.index(".spawn(move ||"), service.index("store.execute(request.method)"))
        for path in HOT_PATH_SOURCES:
            code = production_code(path.read_text(encoding="utf-8"))
            self.assertNotIn("task_store", code, str(path))
            self.assertNotIn("task_service", code, str(path))

    def test_tasks_panel_is_endpoint_only_with_cached_pure_presentation(self) -> None:
        tasks = PROJECT_ROOT / "src/client/shell/tasks.rs"
        rules = (
            (re.compile(r"\b(?:std|tokio)::(?:fs|process)\b"), "client filesystem/process access"),
            (re.compile(r"\b(?:read_dir|canonicalize|decode|spawn|Command)\s*\("), "client IO/decode"),
        )
        board = PROJECT_ROOT / "src/client/shell/tasks/kanban.rs"
        self.assertEqual(find_violations((tasks, board), rules), [])
        source = tasks.read_text(encoding="utf-8")
        tick = source[source.index("pub(super) fn tick_tasks"):source.index("pub(super) fn complete_tasks")]
        self.assertLess(tick.index("self.active_tasks_surface().is_none()"), tick.index("push_endpoint_method_with_kind"))
        board_source = board.read_text(encoding="utf-8")
        surface = board_source[board_source.index("fn active_tasks_surface"):board_source.index("fn open_kanban")]
        self.assertIn("ClientShellOverlay::Kanban", surface)
        self.assertIn("InspectorView::Tasks && self.inspector_visible()", surface)
        self.assertIn("None", surface)
        self.assertNotIn("push_endpoint_method", board_source)
        board_render = board_source[board_source.index("fn render_kanban"):board_source.index("fn route_kanban_mouse")]
        for forbidden in (".prepare(", "UnicodeWidthStr::width", ".lines()", "&mut self"):
            self.assertNotIn(forbidden, board_render)
        self.assertNotIn(".prepare()", tick)
        render = source[source.index("pub(super) fn render_tasks"):source.index("fn select_task")]
        for forbidden in (".prepare()", "UnicodeWidthStr::width", ".lines()", "&mut self"):
            self.assertNotIn(forbidden, render)
        for guard in ("active_snapshot_generation", "target.generation", "target.selected", "source.store_id", "source.reported.boot_id", "agent_session.is_none()"):
            self.assertIn(guard, source)

    def test_tasks_do_not_extend_wire_snapshots_events_or_handoff_manifests(self) -> None:
        for relative in ("src/protocol/wire.rs", "src/persist/snapshot.rs", "src/api/schema/events.rs", "src/api/schema/server.rs", "src/server/handoff.rs"):
            code = production_code((PROJECT_ROOT / relative).read_text(encoding="utf-8"))
            self.assertNotRegex(code, r"\b(?:TaskInfo|TaskStore|TaskStatus|TaskConclusion|TaskCreated|TaskUpdated|task_store|task_service)\b", relative)
        lifecycle = (PROJECT_ROOT / "src/server/headless/lifecycle.rs").read_text(encoding="utf-8")
        self.assertLess(lifecycle.index("task_service.quiesce("), lifecycle.index("spawn_handoff_import("))
        self.assertIn("self.app.task_service.resume()", lifecycle)

    def test_scanner_ignores_non_production_references(self) -> None:
        source = '''
// runtime.input_state()
const EXAMPLE: &str = "runtime.input_state()";
#[cfg(test)]
mod tests {
    fn aggregate_state_test() { runtime.input_state(); }
}
fn production_after_tests() {}
'''
        code = production_code(source)
        self.assertNotRegex(code, FORBIDDEN_CALLS[0][0])
        self.assertIn("fn production_after_tests()", code)
        self.assertEqual(code.count("\n"), source.count("\n"))

    def test_scanner_checks_production_after_test_modules(self) -> None:
        source = '''
#[cfg(test)]
mod tests {
    const BRACES: &str = "}}";
}
fn render() { TerminalRuntime::input_state; }
'''
        self.assertRegex(production_code(source), FORBIDDEN_CALLS[0][0])

    def test_scanner_catches_each_aggregate_state_call(self) -> None:
        cases = (
            ("fn render() { runtime.input_state(); }", INPUT_STATE_CALL),
            ("fn render() { runtime.keyboard_state_ansi(); }", KEYBOARD_STATE_ANSI_CALL),
            ("fn render() { runtime.kitty_keyboard_state_ansi(); }", KEYBOARD_STATE_ANSI_CALL),
        )
        for source, pattern in cases:
            with self.subTest(source=source):
                self.assertRegex(production_code(source), pattern)

    def test_scanner_catches_imported_process_query(self) -> None:
        source = "fn render() { foreground_job(pid); }"
        self.assertRegex(production_code(source), FORBIDDEN_CALLS[3][0])


if __name__ == "__main__":
    unittest.main()
