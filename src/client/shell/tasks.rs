//! Endpoint-only task presentation. Reports are untrusted claims, not executed checks.
use super::inspector::{clean, clean_lines, crop_cells, InspectorRegion, InspectorView};
use super::*;
use crate::api::schema::{
    Method, ResponseResult, TaskInfo, TaskListParams, TaskSource, TaskSummary,
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};

mod kanban;

const PAGE_SIZE: usize = 50;
const REFRESH: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
struct TaskContext {
    endpoint: ClientEndpointId,
    boot: String,
    connection: Option<u64>,
    // None is All, not a dependency on a live pane or its cwd.
    workspace: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct TaskTarget {
    context: TaskContext,
    generation: u64,
    selected: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) enum TaskRead {
    List,
    Detail(String),
}

#[derive(Default)]
pub(super) struct Tasks {
    context: Option<TaskContext>,
    generation: u64,
    all: bool,
    store: Option<String>,
    summaries: Vec<TaskSummary>,
    after: Option<String>,
    previous: Vec<Option<String>>,
    next: Option<String>,
    selected: Option<String>,
    detail: Option<TaskInfo>,
    list_needed: bool,
    detail_needed: bool,
    last_refresh: Option<Instant>,
    message: String,
    // Prepared only on actual data/selection changes, never in render/layout/timer loops.
    list_lines: Vec<String>,
    detail_lines: Vec<String>,
    links: HashMap<usize, TaskSource>,
    list_columns: usize,
    detail_columns: usize,
    board: kanban::Kanban,
    board_pointer_capture: bool,
    #[cfg(test)]
    builds: usize,
}

impl Tasks {
    fn prepare(&mut self) {
        self.list_lines = self
            .summaries
            .iter()
            .map(|task| {
                format!(
                    "{}{} | rpt:{} | {}",
                    if self.selected.as_ref() == Some(&task.task_id) {
                        "> "
                    } else {
                        "  "
                    },
                    clean(&task.status),
                    clean(&task.reported_verification),
                    clean(&task.title)
                )
            })
            .collect();
        self.board.prepare(&self.summaries);
        self.detail_lines.clear();
        self.links.clear();
        if let Some(task) = &self.detail {
            let lines = &mut self.detail_lines;
            lines.push(clean(&task.title));
            lines.push(format!(
                "Status: {} | revision: {}",
                clean(&task.status),
                task.revision
            ));
            lines.push(format!(
                "Reported verification: {}",
                clean(&task.reported_verification)
            ));
            lines.push("Agent report only; Herdr did not run checks.".into());
            lines.extend(
                clean_lines(
                    task.verification_evidence
                        .as_deref()
                        .unwrap_or("No verification evidence reported."),
                )
                .lines()
                .map(str::to_owned),
            );
            lines.push(format!(
                "Workspace: {}",
                clean(
                    task.workspace_label
                        .as_deref()
                        .or(task.workspace_id.as_deref())
                        .unwrap_or("unassigned")
                )
            ));
            lines.extend(
                clean_lines(task.description.as_deref().unwrap_or("No description."))
                    .lines()
                    .map(str::to_owned),
            );
            append_source(lines, &mut self.links, &task.source);
            lines.push("Conclusions (chronological; saved reports):".into());
            if task.conclusions.is_empty() {
                lines.push("No conclusions reported.".into());
            }
            let mut conclusions = task.conclusions.iter().collect::<Vec<_>>();
            conclusions.sort_by_key(|entry| entry.recorded_at_unix_ms);
            for entry in conclusions {
                lines.push(format!(
                    "-- {} @ {} ms",
                    clean(&entry.conclusion_id),
                    entry.recorded_at_unix_ms
                ));
                lines.extend(clean_lines(&entry.text).lines().map(str::to_owned));
                append_source(lines, &mut self.links, &entry.source);
            }
        }
        self.list_columns = self.list_lines.iter().map(|s| s.width()).max().unwrap_or(0);
        self.detail_columns = self
            .detail_lines
            .iter()
            .map(|s| s.width())
            .max()
            .unwrap_or(0);
        #[cfg(test)]
        {
            self.builds += 1;
        }
    }
}

fn append_source(
    lines: &mut Vec<String>,
    links: &mut HashMap<usize, TaskSource>,
    source: &TaskSource,
) {
    let reported = &source.reported;
    lines.push(format!(
        "Reported by: {} | agent: {} | session: {}",
        clean(reported.reporter.as_deref().unwrap_or("unspecified")),
        clean(reported.agent.as_deref().unwrap_or("unspecified")),
        clean(reported.agent_session.as_deref().unwrap_or("unspecified"))
    ));
    lines.push(format!(
        "Reported boot: {} | pane: {}",
        clean(reported.boot_id.as_deref().unwrap_or("unspecified")),
        clean(reported.pane_id.as_deref().unwrap_or("unspecified"))
    ));
    links.insert(lines.len(), source.clone());
    lines.push("[Focus source] best effort; not a historical row".into());
}

impl ClientShellState {
    pub(super) fn invalidate_tasks(&mut self) {
        self.tasks.generation = self.tasks.generation.wrapping_add(1);
        self.tasks.list_needed = true;
        self.tasks.detail_needed = self.tasks.selected.is_some();
    }

    fn task_context(&self) -> Option<TaskContext> {
        if !self.endpoint_is_online(&self.active_endpoint_id) {
            return None;
        }
        let snapshot = self.snapshot.as_deref()?;
        let workspace = if self.tasks.all {
            None
        } else {
            Some(snapshot.focused_workspace_id.clone()?)
        };
        Some(TaskContext {
            endpoint: self.active_endpoint_id.clone(),
            boot: snapshot.boot_id.clone(),
            connection: self.active_snapshot_generation,
            workspace,
        })
    }

    pub(super) fn tick_tasks(&mut self, outcome: &mut ClientShellInput) {
        if self.active_tasks_surface().is_none() {
            return;
        }
        let context = self.task_context();
        if self.tasks.context != context {
            self.cancel_inspector_drag();
            self.tasks = Tasks {
                all: self.tasks.all,
                board_pointer_capture: self.tasks.board_pointer_capture,
                context: context.clone(),
                generation: self.tasks.generation.wrapping_add(1),
                list_needed: true,
                ..Tasks::default()
            };
            if self.active_tasks_surface() == Some(kanban::TasksSurface::Inspector) {
                self.inspector.list_scroll = 0;
                self.inspector.preview_scroll = 0;
                self.inspector.list_horizontal_scroll = 0;
                self.inspector.preview_horizontal_scroll = 0;
            }
            outcome.repaint = true;
        }
        let Some(context) = context else {
            return;
        };
        // Both methods must be explicitly advertised, even on Local. Never probe a legacy server.
        let supported = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == context.endpoint)
            .and_then(|endpoint| endpoint.methods.as_ref())
            .is_some_and(|methods| {
                ["task.list", "task.get"]
                    .iter()
                    .all(|name| methods.iter().any(|method| method == name))
            });
        if !supported {
            let message = "Tasks unavailable: server must advertise task.list and task.get.";
            if self.tasks.message != message {
                self.tasks.message = message.into();
                outcome.repaint = true;
            }
            return;
        }
        if self
            .pending_requests
            .values()
            .any(|request| matches!(request.kind, PendingEndpointKind::Tasks { .. }))
        {
            return;
        }
        let read = if self.tasks.detail_needed {
            self.tasks.selected.clone().map(TaskRead::Detail)
        } else if self.tasks.list_needed
            || self
                .tasks
                .last_refresh
                .is_none_or(|last| last.elapsed() >= REFRESH)
        {
            Some(TaskRead::List)
        } else {
            None
        };
        let Some(read) = read else {
            return;
        };
        let method = match &read {
            TaskRead::List => Method::TaskList(TaskListParams {
                workspace_id: context.workspace.clone(),
                after: self.tasks.after.clone(),
                limit: Some(PAGE_SIZE),
                status: None,
            }),
            TaskRead::Detail(task_id) => Method::TaskGet(crate::api::schema::TaskTarget {
                task_id: task_id.clone(),
            }),
        };
        let target = TaskTarget {
            context,
            generation: self.tasks.generation,
            selected: self.tasks.selected.clone(),
        };
        if self.push_endpoint_method_with_kind(
            method,
            PendingEndpointKind::Tasks {
                target,
                read: read.clone(),
            },
            outcome,
        ) {
            match read {
                TaskRead::List => {
                    self.tasks.list_needed = false;
                    self.tasks.last_refresh = Some(Instant::now());
                }
                TaskRead::Detail(_) => self.tasks.detail_needed = false,
            }
        }
    }

    pub(super) fn complete_tasks(
        &mut self,
        target: TaskTarget,
        read: TaskRead,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        if result
            .as_ref()
            .err()
            .is_some_and(|error| error.code.as_deref() == Some("endpoint_cancelled"))
        {
            return (false, Vec::new());
        }
        if self.active_tasks_surface().is_none()
            || self.task_context().as_ref() != Some(&target.context)
            || self.tasks.context.as_ref() != Some(&target.context)
            || self.tasks.generation != target.generation
            || self.tasks.selected != target.selected
        {
            return (false, Vec::new());
        }
        match (read, result) {
            (
                TaskRead::List,
                Ok(ResponseResult::TaskList {
                    store_id,
                    tasks,
                    next_after,
                }),
            ) => {
                let store_changed = self
                    .tasks
                    .store
                    .as_ref()
                    .is_some_and(|store| store != &store_id);
                let changed = self.tasks.store.as_ref() != Some(&store_id)
                    || self.tasks.summaries != tasks
                    || self.tasks.next != next_after;
                if store_changed {
                    self.tasks.detail = None;
                    self.tasks.selected = None;
                }
                self.tasks.store = Some(store_id);
                if let Some(selected) = &self.tasks.selected {
                    if let Some(summary) = tasks.iter().find(|task| &task.task_id == selected) {
                        self.tasks.detail_needed = self
                            .tasks
                            .detail
                            .as_ref()
                            .is_none_or(|detail| detail.revision != summary.revision);
                    } else {
                        self.tasks.selected = None;
                        self.tasks.detail = None;
                    }
                }
                self.tasks.summaries = tasks;
                self.tasks.next = next_after;
                if self.tasks.message.starts_with("Task list rejected:") {
                    self.tasks.message.clear();
                }
                if changed {
                    self.tasks.prepare();
                }
            }
            (TaskRead::Detail(id), Ok(ResponseResult::TaskRecord { store_id, task, .. }))
                if self.tasks.store.as_ref() == Some(&store_id)
                    && task.task_id == id
                    && task.source.store_id == store_id =>
            {
                if self.tasks.detail.as_ref() != Some(&task) {
                    self.tasks.detail = Some(task);
                    self.tasks.prepare();
                }
                self.tasks.message.clear();
            }
            (read, Err(error)) => {
                let kind = if matches!(read, TaskRead::List) {
                    "list"
                } else {
                    "detail"
                };
                self.tasks.message = format!("Task {kind} rejected: {}", clean(&error.message));
            }
            _ => {
                self.tasks.message =
                    "Task response/store mismatch; source navigation disabled.".into();
                self.tasks.detail = None;
                self.tasks.prepare();
            }
        }
        let mut outcome = ClientShellInput::default();
        self.tick_tasks(&mut outcome);
        (true, outcome.actions)
    }

    pub(super) fn task_regions(
        &self,
        area: Rect,
        top: usize,
        left: usize,
        detail_top: usize,
        detail_left: usize,
    ) -> (InspectorRegion, InspectorRegion) {
        let list = Self::inspector_list_area(area);
        let detail = Rect::new(
            area.x + 1,
            list.bottom() + 1,
            area.width.saturating_sub(2),
            area.bottom().saturating_sub(list.bottom() + 1),
        );
        (
            InspectorRegion::new(
                list,
                self.tasks.list_lines.len(),
                self.tasks.list_columns,
                top,
                left,
            ),
            InspectorRegion::new(
                detail,
                self.tasks.detail_lines.len(),
                self.tasks.detail_columns,
                detail_top,
                detail_left,
            ),
        )
    }

    pub(super) fn render_tasks(&self, buffer: &mut Buffer, area: Rect) {
        if !self.retired_presentation_enabled() {
            return;
        }
        let style = Style::default()
            .fg(self.config.palette.text)
            .bg(self.config.palette.panel_bg);
        let put = |buffer: &mut Buffer, y, text: &str| {
            render::put_text(
                buffer,
                area.x + 1,
                y,
                area.width.saturating_sub(2),
                text,
                style,
            )
        };
        if self.tasks.context != self.task_context() {
            put(buffer, area.y + 3, "Waiting for current task scope...");
            return;
        }
        put(
            buffer,
            area.y + 2,
            if self.tasks.all {
                " Workspace  [All tasks]"
            } else {
                "[Workspace]  All tasks"
            },
        );
        let busy = self
            .pending_requests
            .values()
            .any(|request| matches!(request.kind, PendingEndpointKind::Tasks { .. }));
        let message = if !self.tasks.message.is_empty() {
            self.tasks.message.as_str()
        } else if self.task_context().is_none() {
            "No connected workspace. Choose All tasks."
        } else if busy {
            "Loading task reports..."
        } else if self.tasks.summaries.is_empty() {
            "No tasks in this scope."
        } else {
            "Status | rpt: agent claim"
        };
        put(buffer, area.y + 3, message);
        put(
            buffer,
            area.y + 4,
            &format!(
                "{} reports | page {}",
                self.tasks.summaries.len(),
                self.tasks.previous.len() + 1
            ),
        );
        if self.tasks.after.is_some() {
            render::put_text(buffer, area.right() - 14, area.y + 4, 6, "[prev]", style);
        }
        if self.tasks.next.is_some() {
            render::put_text(buffer, area.right() - 7, area.y + 4, 6, "[next]", style);
        }
        let (list, detail) = self.task_regions(
            area,
            self.inspector.list_scroll,
            self.inspector.list_horizontal_scroll,
            self.inspector.preview_scroll,
            self.inspector.preview_horizontal_scroll,
        );
        for (region, lines) in [
            (list, &self.tasks.list_lines),
            (detail, &self.tasks.detail_lines),
        ] {
            for (index, line) in lines
                .iter()
                .skip(region.top)
                .take(usize::from(region.content.height))
                .enumerate()
            {
                render::put_text(
                    buffer,
                    region.content.x,
                    region.content.y + index as u16,
                    region.content.width,
                    &crop_cells(line, region.left, region.content.width),
                    style,
                );
            }
            region.paint(buffer, &self.config.palette);
        }
        put(
            buffer,
            list.content.y + list.content.height + u16::from(!list.horizontal.is_empty()),
            if self.tasks.selected.is_none() {
                "-- Select a task for saved detail"
            } else {
                "-- Saved detail / conclusions"
            },
        );
    }

    fn select_task(&mut self, id: String) {
        self.tasks.selected = Some(id);
        self.tasks.detail = None;
        self.tasks.detail_needed = true;
        self.tasks.generation = self.tasks.generation.wrapping_add(1);
        self.tasks.message.clear();
        self.tasks.board.detail_top = 0;
        self.tasks.board.detail_left = 0;
        if self.active_tasks_surface() == Some(kanban::TasksSurface::Inspector) {
            self.inspector.preview_scroll = 0;
            self.inspector.preview_horizontal_scroll = 0;
        }
        self.tasks.prepare();
    }

    fn focus_task_source(&mut self, source: &TaskSource, outcome: &mut ClientShellInput) {
        let valid_context = self.task_context().as_ref() == self.tasks.context.as_ref()
            && self.tasks.context.is_some();
        let valid = valid_context && self.tasks.store.as_ref() == Some(&source.store_id) && self.snapshot.as_deref().is_some_and(|snapshot| {
            source.reported.boot_id.as_deref() == Some(snapshot.boot_id.as_str())
                && source.reported.pane_id.as_ref().is_some_and(|id| snapshot.panes.iter().any(|pane| &pane.pane_id == id))
                // Snapshot has no trustworthy agent-session identity. Never guess a match.
                && source.reported.agent_session.is_none()
                && source.reported.agent.as_ref().is_none_or(|agent| snapshot.agents.iter().any(|live| Some(&live.pane_id) == source.reported.pane_id.as_ref() && live.agent.as_ref() == Some(agent)))
        });
        match (valid, source.reported.pane_id.as_ref()) {
            (true, Some(pane_id)) => {
                self.push_endpoint_method(
                    Method::PaneFocus(crate::api::schema::PaneTarget {
                        pane_id: pane_id.clone(),
                    }),
                    outcome,
                );
            }
            _ => {
                self.tasks.message =
                    "Source unavailable or changed; saved conclusion retained.".into()
            }
        }
    }

    pub(super) fn handle_tasks_mouse(
        &mut self,
        mouse: MouseEvent,
        area: Rect,
        list: InspectorRegion,
        detail: InspectorRegion,
        outcome: &mut ClientShellInput,
    ) {
        if !self.retired_presentation_enabled() {
            return;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if mouse.row == area.y {
                self.tasks.message.clear();
                self.invalidate_tasks();
            } else if mouse.row == area.y + 2 {
                self.tasks.all = mouse.column >= area.x + 12;
                self.invalidate_tasks();
            } else if mouse.row == area.y + 4 {
                let next = mouse.column >= area.right() - 7;
                if next && self.tasks.next.is_some() {
                    self.tasks.previous.push(self.tasks.after.clone());
                    self.tasks.after = self.tasks.next.clone();
                } else if !next && mouse.column >= area.right() - 14 && self.tasks.after.is_some() {
                    self.tasks.after = self.tasks.previous.pop().flatten();
                } else {
                    return;
                }
                self.tasks.selected = None;
                self.tasks.detail = None;
                self.tasks.summaries.clear();
                self.tasks.next = None;
                self.tasks.detail_needed = false;
                self.tasks.prepare();
                self.inspector.list_scroll = 0;
                self.inspector.preview_scroll = 0;
                self.invalidate_tasks();
            } else if contains(list.content, (mouse.column, mouse.row)) {
                if let Some(task) = self
                    .tasks
                    .summaries
                    .get(list.top + usize::from(mouse.row - list.content.y))
                {
                    self.select_task(task.task_id.clone());
                }
            } else if contains(detail.content, (mouse.column, mouse.row))
                && detail.left + usize::from(mouse.column - detail.content.x)
                    < "[Focus source]".len()
            {
                if let Some(source) = self
                    .tasks
                    .links
                    .get(&(detail.top + usize::from(mouse.row - detail.content.y)))
                    .cloned()
                {
                    self.focus_task_source(&source, outcome);
                }
            }
        } else if matches!(
            mouse.kind,
            MouseEventKind::ScrollDown
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ) {
            let is_list = contains(Self::inspector_list_area(area), (mouse.column, mouse.row));
            let region = if is_list { list } else { detail };
            let horizontal = matches!(
                mouse.kind,
                MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight
            );
            let offset = match (is_list, horizontal) {
                (true, true) => &mut self.inspector.list_horizontal_scroll,
                (true, false) => &mut self.inspector.list_scroll,
                (false, true) => &mut self.inspector.preview_horizontal_scroll,
                (false, false) => &mut self.inspector.preview_scroll,
            };
            *offset = if matches!(
                mouse.kind,
                MouseEventKind::ScrollDown | MouseEventKind::ScrollRight
            ) {
                offset
                    .saturating_add(3)
                    .min(region.metrics(horizontal).max_offset_from_bottom)
            } else {
                offset.saturating_sub(3)
            };
        }
    }
}

#[cfg(test)]
mod tests;
