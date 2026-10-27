//! Client-local, mouse-first read-only browser. No filesystem/process access is
//! permitted here: even Local uses the same advertised endpoint read APIs.
use super::*;
use crate::api::schema::{
    FileEntry, FilePathParams, FilePreviewContent, FilePreviewParams, GitDiffParams, GitFileEntry,
    GitReadParams, Method, PreviewRaster, ResponseResult,
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

const DEFAULT_WIDTH: u16 = 42;
const MIN_WIDTH: u16 = 28;
const MIN_TERMINAL_WIDTH: u16 = 40;
const MAX_EXPANDED: usize = 32;

pub(super) fn reserve_area(
    layout: &mut ClientShellLayout,
    open: bool,
    preferred_width: Option<u16>,
) {
    if !open
        || !layout.mobile_header.is_empty()
        || layout.pane_surface.width < MIN_WIDTH + MIN_TERMINAL_WIDTH
        || layout.pane_surface.height < 8
    {
        return;
    }
    let width = preferred_width
        .unwrap_or(DEFAULT_WIDTH)
        .clamp(MIN_WIDTH, layout.pane_surface.width - MIN_TERMINAL_WIDTH);
    let right = layout.pane_surface.right();
    layout.pane_surface.width -= width;
    layout.tab_bar.width = layout.tab_bar.width.saturating_sub(width);
    layout.inspector = Rect::new(right - width, 0, width, layout.sidebar.height);
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InspectorSource {
    endpoint: ClientEndpointId,
    boot: String,
    connection: Option<u64>,
    pane: String,
    cwd: String,
}

#[derive(Clone, Debug)]
pub(super) struct FileLinkAttempt {
    source: InspectorSource,
    focused_pane: Option<String>,
    generation: u64,
    params: crate::api::schema::PaneLinkActivateParams,
}

pub(super) struct PendingFileOpen {
    attempt: FileLinkAttempt,
    reference: crate::api::schema::FileReference,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct InspectorTarget {
    source: InspectorSource,
    generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum InspectorRead {
    List(String),
    File {
        root: String,
        path: String,
    },
    Status,
    Diff {
        path: String,
        original_path: Option<String>,
        staged: bool,
    },
    Log,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum InspectorView {
    #[default]
    Files,
    Git,
    Log,
    Tasks,
}

#[derive(Default)]
pub(super) struct Inspector {
    pub(super) open: bool,
    source: Option<InspectorSource>,
    generation: u64,
    pub(super) view: InspectorView,
    queue: VecDeque<InspectorRead>,
    directories: HashMap<String, Vec<FileEntry>>,
    expanded: HashSet<String>,
    files_root: String,
    git_root: String,
    branch: String,
    changes: Vec<GitFileEntry>,
    log: String,
    preview: String,
    selection: String,
    message: String,
    pub(super) list_scroll: usize,
    pub(super) preview_scroll: usize,
    pub(super) list_horizontal_scroll: usize,
    pub(super) preview_horizontal_scroll: usize,
    // Session-only preference, retained across source changes and responsive layouts.
    pub(super) preferred_width: Option<u16>,
    pointer_capture: bool,
    drag: Option<InspectorDrag>,
    preview_line: Option<u32>,
    preview_column: Option<u32>,
    raster: Option<CachedRaster>,
    page: u32,
    pages: u32,
    preview_file: Option<(String, String)>,
    raster_bounds: Option<(u32, u32)>,
    // Prepared outside render; invalidated only when rows/text/column semantics change.
    content: Option<InspectorContent>,
    #[cfg(test)]
    content_builds: usize,
}

#[derive(Default)]
struct InspectorContent {
    list: Vec<(String, InspectorRead)>,
    list_columns: usize,
    preview_rows: usize,
    preview_columns: usize,
}

#[derive(Clone, Copy)]
enum InspectorDrag {
    Width,
    Scrollbar {
        list: bool,
        horizontal: bool,
        grab_offset: u16,
        last_position: u16,
    },
}

#[derive(Clone, Copy)]
pub(super) struct InspectorRegion {
    pub(super) content: Rect,
    pub(super) vertical: Rect,
    pub(super) horizontal: Rect,
    pub(super) rows: usize,
    pub(super) columns: usize,
    pub(super) top: usize,
    pub(super) left: usize,
}

impl InspectorRegion {
    pub(super) fn new(area: Rect, rows: usize, columns: usize, top: usize, left: usize) -> Self {
        let mut content = area;
        // Adding one bar can make the other necessary. Reserve both axes to a
        // fixed point so paint, hit testing, and offset limits share geometry.
        for _ in 0..3 {
            content.width = area.width.saturating_sub(u16::from(
                area.width > 1 && rows > usize::from(content.height),
            ));
            // A scrollbar must never consume the last content row (or column).
            content.height = area.height.saturating_sub(u16::from(
                area.height > 1 && columns > usize::from(content.width),
            ));
        }
        let vertical = if content.width < area.width && content.height > 0 {
            Rect::new(content.right(), content.y, 1, content.height)
        } else {
            Rect::default()
        };
        let horizontal = if content.height < area.height && content.width > 0 {
            Rect::new(content.x, content.bottom(), content.width, 1)
        } else {
            Rect::default()
        };
        Self {
            content,
            vertical,
            horizontal,
            rows,
            columns,
            top: top.min(rows.saturating_sub(usize::from(content.height))),
            left: left.min(columns.saturating_sub(usize::from(content.width))),
        }
    }

    pub(super) fn metrics(self, horizontal: bool) -> crate::pane::ScrollMetrics {
        let (total, viewport, offset) = if horizontal {
            (self.columns, usize::from(self.content.width), self.left)
        } else {
            (self.rows, usize::from(self.content.height), self.top)
        };
        let max = total.saturating_sub(viewport);
        // Shared scrollbar primitives are bottom-based; inspector offsets are
        // top/left-based. Transpose the horizontal axis, not its direction.
        crate::pane::ScrollMetrics {
            offset_from_bottom: max.saturating_sub(offset),
            max_offset_from_bottom: max,
            viewport_rows: viewport,
        }
    }

    pub(super) fn track(self, horizontal: bool) -> Rect {
        if horizontal {
            Rect::new(0, self.horizontal.x, 1, self.horizontal.width)
        } else {
            self.vertical
        }
    }

    pub(super) fn paint(self, buffer: &mut Buffer, palette: &crate::app::state::Palette) {
        if !self.vertical.is_empty() {
            crate::ui::render_pane_scrollbar_buffer(
                buffer,
                self.metrics(false),
                self.vertical,
                palette,
                true,
            );
        }
        if !self.horizontal.is_empty() {
            if let Some(thumb) = crate::ui::scrollbar_thumb(self.metrics(true), self.track(true)) {
                for x in self.horizontal.x..self.horizontal.right() {
                    let active = (thumb.top..thumb.top + thumb.len).contains(&x);
                    buffer[(x, self.horizontal.y)]
                        .set_symbol(if active { "━" } else { "─" })
                        .set_fg(if active {
                            palette.overlay1
                        } else {
                            palette.overlay0
                        });
                }
            }
        }
    }
}

// Preserve the existing file-link column convention (one-based Unicode scalar
// index, applied per line). User horizontal scrolling is separately cell-based.
fn reference_line(line: &str, column: Option<u32>) -> &str {
    let skip = column.unwrap_or(1).saturating_sub(1) as usize;
    let byte = line
        .char_indices()
        .nth(skip)
        .map_or(line.len(), |(byte, _)| byte);
    &line[byte..]
}

pub(super) fn crop_cells(text: &str, left: usize, width: u16) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let right = left.saturating_add(usize::from(width));
    let mut column = 0;
    let mut result = String::new();
    for grapheme in text.graphemes(true) {
        let end = column + UnicodeWidthStr::width(grapheme);
        if column >= right {
            break;
        }
        if end > left {
            if column >= left && end <= right {
                result.push_str(grapheme);
            } else {
                result.extend(std::iter::repeat_n(' ', end.min(right) - column.max(left)));
            }
        }
        column = end;
    }
    result
}

struct CachedRaster {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

fn validate_raster(raster: PreviewRaster) -> Result<CachedRaster, &'static str> {
    use base64::Engine;
    if raster.width == 0
        || raster.height == 0
        || raster.width > 160
        || raster.height > 160
        || raster.rgb_base64.len() > 102400
    {
        return Err("invalid raster dimensions/size");
    }
    let rgb = base64::engine::general_purpose::STANDARD
        .decode(raster.rgb_base64)
        .map_err(|_| "invalid raster encoding")?;
    if rgb.len() != raster.width as usize * raster.height as usize * 3 {
        return Err("invalid raster pixel count");
    }
    Ok(CachedRaster {
        width: raster.width,
        height: raster.height,
        rgb,
    })
}

pub(super) fn clean(text: &str) -> String {
    text.chars().flat_map(|ch| {
        if ch.is_control() || matches!(ch, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            ch.escape_default().collect::<Vec<_>>()
        } else { vec![ch] }
    }).collect()
}

pub(super) fn clean_lines(text: &str) -> String {
    text.split('\n').map(clean).collect::<Vec<_>>().join("\n")
}

fn relative_reference(root: &str, value: &str) -> Option<String> {
    use std::path::{Component, Path};
    let value = value.strip_prefix("./").unwrap_or(value);
    let path = Path::new(value);
    let relative = if path.is_absolute() {
        path.strip_prefix(Path::new(root)).ok()?
    } else {
        path
    };
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return None;
    }
    Some(relative.to_str()?.to_owned())
}

fn effective_cwd(pane: &crate::protocol::ClientShellPane) -> Option<&str> {
    pane.foreground_cwd
        .as_deref()
        .filter(|cwd| !cwd.is_empty())
        .or_else(|| pane.cwd.as_deref().filter(|cwd| !cwd.is_empty()))
}

impl ClientShellState {
    fn file_source_live(&self, source: &InspectorSource) -> bool {
        if self.active_endpoint_id != source.endpoint
            || !self.endpoint_is_online(&source.endpoint)
            || self.active_snapshot_generation != source.connection
        {
            return false;
        }
        self.snapshot.as_deref().is_some_and(|snapshot| {
            snapshot.boot_id == source.boot
                && snapshot
                    .panes
                    .iter()
                    .find(|pane| pane.pane_id == source.pane)
                    .and_then(effective_cwd)
                    == Some(source.cwd.as_str())
        })
    }

    pub(super) fn supports_file_link(&self) -> bool {
        if !self.retired_presentation_enabled() {
            return false;
        }
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .and_then(|endpoint| endpoint.methods.as_ref())
            .is_some_and(|methods| {
                methods.contains("pane.file_link.activate") && methods.contains("file.preview")
            })
    }

    pub(super) fn file_link_attempt(
        &self,
        params: crate::api::schema::PaneLinkActivateParams,
    ) -> Option<FileLinkAttempt> {
        if !self.supports_file_link() {
            return None;
        }
        let snapshot = self.snapshot.as_deref()?;
        let pane = snapshot
            .panes
            .iter()
            .find(|pane| pane.pane_id == params.pane_id)?;
        Some(FileLinkAttempt {
            source: InspectorSource {
                endpoint: self.active_endpoint_id.clone(),
                boot: snapshot.boot_id.clone(),
                connection: self.active_snapshot_generation,
                pane: pane.pane_id.clone(),
                cwd: effective_cwd(pane)?.to_owned(),
            },
            focused_pane: snapshot.focused_pane_id.clone(),
            generation: self.inspector.generation,
            params,
        })
    }

    pub(super) fn file_link_attempt_current(&self, attempt: &FileLinkAttempt) -> bool {
        if !self.retired_presentation_enabled() {
            return false;
        }
        self.file_source_live(&attempt.source)
            && self.inspector.generation == attempt.generation
            && self.focused_pane_id() == attempt.focused_pane
    }

    pub(super) fn show_file_link_retry_notice(&mut self) -> bool {
        self.push_endpoint_notice(
            ClientEndpointNoticeKind::Rejected,
            "file_link_changed",
            "File link changed",
            "Pane content or target changed. Try Ctrl+clicking the filename again.",
        )
    }

    pub(super) fn request_file_link(
        &mut self,
        attempt: FileLinkAttempt,
        inner_rect: Rect,
        fallback_events: Vec<MouseEvent>,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !self.file_link_attempt_current(&attempt) {
            return false;
        }
        self.push_endpoint_method_with_kind(
            Method::PaneFileLinkActivate(attempt.params.clone()),
            PendingEndpointKind::FileLinkActivate {
                attempt,
                inner_rect,
                fallback_events,
            },
            outcome,
        )
    }

    pub(super) fn complete_file_link(
        &mut self,
        attempt: FileLinkAttempt,
        inner_rect: Rect,
        fallback_events: Vec<MouseEvent>,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        let before_release = !fallback_events
            .iter()
            .any(|event| matches!(event.kind, MouseEventKind::Up(_)));
        if !self.file_link_attempt_current(&attempt)
            || result
                .as_ref()
                .err()
                .is_some_and(|error| error.code.as_deref() == Some("endpoint_cancelled"))
        {
            self.url_click_consumes_until_up = before_release;
            return (false, Vec::new());
        }
        if result.as_ref().err().is_some_and(|error| {
            matches!(
                error.code.as_deref(),
                Some("stale_content" | "stale_target")
            )
        }) {
            self.url_click_consumes_until_up = before_release;
            return (self.show_file_link_retry_notice(), Vec::new());
        }
        if let Ok(ResponseResult::FileReference {
            reference: Some(reference),
        }) = result
        {
            if relative_reference(&attempt.source.cwd, &reference.path).is_some() {
                let mut outcome = ClientShellInput::default();
                if self.focused_pane_id().as_deref() != Some(attempt.source.pane.as_str()) {
                    self.push_endpoint_method(
                        Method::PaneFocus(crate::api::schema::PaneTarget {
                            pane_id: attempt.source.pane.clone(),
                        }),
                        &mut outcome,
                    );
                }
                self.pending_file_open = Some(PendingFileOpen { attempt, reference });
                self.url_click_consumes_until_up = before_release;
                return (true, outcome.actions);
            }
        }
        let can_replay = self.overlay.is_none()
            && self.mode == ClientShellMode::Terminal
            && self
                .hits
                .panes
                .iter()
                .any(|hit| hit.pane_id == attempt.source.pane && hit.inner_rect == inner_rect);
        (
            false,
            if can_replay {
                vec![ClientShellAction::ReplayMouse(fallback_events)]
            } else {
                self.url_click_consumes_until_up = before_release;
                Vec::new()
            },
        )
    }

    fn file_open_intent_current(&self, pending: &PendingFileOpen) -> bool {
        self.file_source_live(&pending.attempt.source)
            && (self.focused_pane_id() == pending.attempt.focused_pane
                || self.focused_pane_id().as_deref() == Some(pending.attempt.source.pane.as_str()))
    }

    fn open_file_reference(
        &mut self,
        source: InspectorSource,
        reference: crate::api::schema::FileReference,
        outcome: &mut ClientShellInput,
    ) {
        if !self.retired_presentation_enabled() {
            return;
        }
        let Some(path) = relative_reference(&source.cwd, &reference.path) else {
            return;
        };
        let was_open = self.inspector.open;
        self.inspector.open = true;
        self.inspector.view = InspectorView::Files;
        self.reset_inspector_data(Some(source.clone()));
        self.inspector.queue.push_front(InspectorRead::File {
            root: source.cwd,
            path: path.clone(),
        });
        self.inspector.selection = format!(
            "{}:{}:{}",
            path,
            reference.line.unwrap_or(1),
            reference.column.unwrap_or(1)
        );
        self.inspector.preview_line = reference.line;
        self.inspector.preview_column = reference.column;
        if !was_open {
            self.invalidate_pane_surface();
            outcome.resize = true;
        }
        outcome.repaint = true;
        self.update_inspector_geometry_notice();
    }

    fn inspector_source(&self) -> Option<InspectorSource> {
        if !self.endpoint_is_online(&self.active_endpoint_id) {
            return None;
        }
        let snapshot = self.snapshot.as_deref()?;
        let pane_id = snapshot.focused_pane_id.as_ref()?;
        let pane = snapshot
            .panes
            .iter()
            .find(|pane| &pane.pane_id == pane_id)?;
        let cwd = effective_cwd(pane)?;
        Some(InspectorSource {
            endpoint: self.active_endpoint_id.clone(),
            boot: snapshot.boot_id.clone(),
            connection: self.active_snapshot_generation,
            pane: pane_id.clone(),
            cwd: cwd.to_owned(),
        })
    }

    fn inspector_source_current(&self, source: &InspectorSource) -> bool {
        if self.active_endpoint_id != source.endpoint
            || !self.endpoint_is_online(&source.endpoint)
            || self.active_snapshot_generation != source.connection
        {
            return false;
        }
        let Some(snapshot) = self.snapshot.as_deref() else {
            return false;
        };
        snapshot.boot_id == source.boot
            && snapshot.focused_pane_id.as_deref() == Some(source.pane.as_str())
            && snapshot
                .panes
                .iter()
                .find(|pane| pane.pane_id == source.pane)
                .and_then(effective_cwd)
                == Some(source.cwd.as_str())
    }

    pub(super) fn inspector_visible(&self) -> bool {
        self.last_composed_size
            .is_some_and(|(cols, rows)| !self.layout(cols, rows).inspector.is_empty())
    }

    fn update_inspector_geometry_notice(&mut self) {
        if self.inspector.open && self.last_composed_size.is_some() && !self.inspector_visible() {
            self.push_endpoint_notice(
                ClientEndpointNoticeKind::Rejected,
                "inspector_geometry",
                "Inspector needs more space",
                "Widen the window or collapse the left sidebar. Inspector needs 28 + 40 content columns, 8 content rows, and desktop layout.",
            );
        } else {
            self.clear_inspector_geometry_notice();
        }
    }

    fn clear_inspector_geometry_notice(&mut self) -> bool {
        if self
            .visible_endpoint_notice
            .as_ref()
            .is_some_and(|notice| notice.key.code == "inspector_geometry")
        {
            self.visible_endpoint_notice = None;
            true
        } else {
            false
        }
    }

    pub(super) fn toggle_inspector(&mut self, outcome: &mut ClientShellInput) {
        if !self.retired_presentation_enabled() {
            return;
        }
        self.pending_file_open = None;
        self.invalidate_tasks();
        self.inspector.open = !self.inspector.open;
        self.inspector.generation = self.inspector.generation.wrapping_add(1);
        self.inspector.source = None;
        self.inspector.content = None;
        self.inspector.queue.clear();
        self.inspector.drag = None;
        self.invalidate_pane_surface();
        outcome.resize = true;
        outcome.repaint = true;
        self.update_inspector_geometry_notice();
        self.tick_inspector(outcome);
    }

    fn reset_inspector_data(&mut self, source: Option<InspectorSource>) {
        let open = self.inspector.open;
        let generation = self.inspector.generation.wrapping_add(1);
        let view = self.inspector.view;
        let pointer_capture = self.inspector.pointer_capture;
        let preferred_width = self.inspector.preferred_width;
        self.inspector = Inspector {
            open,
            generation,
            view,
            source,
            pointer_capture,
            preferred_width,
            #[cfg(test)]
            content_builds: self.inspector.content_builds,
            ..Inspector::default()
        };
        match view {
            InspectorView::Files => self
                .inspector
                .queue
                .push_back(InspectorRead::List(String::new())),
            InspectorView::Git => self.inspector.queue.push_back(InspectorRead::Status),
            InspectorView::Log => self.inspector.queue.push_back(InspectorRead::Log),
            InspectorView::Tasks => {}
        }
    }

    pub(super) fn sync_inspector_source(&mut self) {
        if !self.retired_presentation_enabled() {
            return;
        }
        if self
            .pending_file_open
            .as_ref()
            .is_some_and(|pending| !self.file_open_intent_current(pending))
        {
            self.pending_file_open = None;
        }
        if self.inspector.open
            && self.inspector.view != InspectorView::Tasks
            && !self
                .inspector
                .source
                .as_ref()
                .is_some_and(|source| self.inspector_source_current(source))
        {
            let source = self.inspector_source();
            if self.inspector.source != source {
                self.reset_inspector_data(source);
            }
        }
    }

    pub(crate) fn tick_inspector(&mut self, outcome: &mut ClientShellInput) {
        if !self.retired_presentation_enabled() {
            return;
        }
        if let Some(pending) = self.pending_file_open.take() {
            if self.file_open_intent_current(&pending) {
                if self.focused_pane_id().as_deref() == Some(pending.attempt.source.pane.as_str()) {
                    self.open_file_reference(pending.attempt.source, pending.reference, outcome);
                } else {
                    self.pending_file_open = Some(pending);
                }
            }
        }
        self.clamp_inspector_scroll();
        self.clamp_kanban_scroll();
        if matches!(self.overlay, Some(ClientShellOverlay::Kanban)) {
            self.tick_tasks(outcome);
            return;
        }
        if !self.inspector_visible() {
            return;
        }
        outcome.repaint |= self.clear_inspector_geometry_notice();
        if self.inspector.view == InspectorView::Tasks {
            self.tick_tasks(outcome);
            return;
        }
        if !self
            .inspector
            .source
            .as_ref()
            .is_some_and(|source| self.inspector_source_current(source))
        {
            let source = self.inspector_source();
            if self.inspector.source != source {
                self.reset_inspector_data(source);
                outcome.repaint = true;
            }
        }
        if self.inspector.source.is_none() {
            return;
        }
        if self
            .pending_requests
            .values()
            .any(|request| matches!(request.kind, PendingEndpointKind::Inspector { .. }))
        {
            return;
        }
        if self.inspector.raster.is_some()
            && !matches!(self.inspector.drag, Some(InspectorDrag::Width))
            && self.inspector.queue.is_empty()
            && self.inspector.raster_bounds != Some(self.preview_bounds())
        {
            if let Some((root, path)) = self.inspector.preview_file.clone() {
                self.inspector.generation = self.inspector.generation.wrapping_add(1);
                self.inspector
                    .queue
                    .push_back(InspectorRead::File { root, path });
            }
        }
        let Some(read) = self.inspector.queue.pop_front() else {
            return;
        };
        let Some(source) = self.inspector.source.clone() else {
            return;
        };
        let method = match &read {
            InspectorRead::List(path) => Method::FileList(FilePathParams {
                root: source.cwd.clone(),
                path: path.clone(),
            }),
            InspectorRead::File { root, path } => {
                let (width, height) = self.preview_bounds();
                self.inspector.raster_bounds = Some((width, height));
                self.inspector.preview_file = Some((root.clone(), path.clone()));
                Method::FilePreview(FilePreviewParams {
                    root: root.clone(),
                    path: path.clone(),
                    page: self.inspector.page.max(1),
                    width,
                    height,
                })
            }
            InspectorRead::Status => Method::GitStatus(GitReadParams {
                cwd: source.cwd.clone(),
            }),
            InspectorRead::Diff {
                path,
                original_path,
                staged,
            } => Method::GitDiff(GitDiffParams {
                cwd: source.cwd.clone(),
                path: path.clone(),
                original_path: original_path.clone(),
                staged: *staged,
            }),
            InspectorRead::Log => Method::GitLog(GitReadParams {
                cwd: source.cwd.clone(),
            }),
        };
        // Absence means unsupported too. Never probe older servers or read locally.
        let name = crate::api::api_method_name(&method);
        if !self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == source.endpoint)
            .and_then(|endpoint| endpoint.methods.as_ref())
            .is_some_and(|methods| methods.iter().any(|method| method == name))
        {
            self.inspector.message = format!("This server does not support {name}");
            self.inspector.queue.clear();
            outcome.repaint = true;
            return;
        }
        let target = InspectorTarget {
            source,
            generation: self.inspector.generation,
        };
        self.push_endpoint_method_with_kind(
            method,
            PendingEndpointKind::Inspector { target, read },
            outcome,
        );
    }

    fn inspector_identity_current(&self, target: &InspectorTarget) -> bool {
        self.inspector.open
            && self.inspector.generation == target.generation
            && self.inspector.source.as_ref() == Some(&target.source)
            && self.inspector_source_current(&target.source)
    }

    fn inspector_target_current(&self, target: &InspectorTarget) -> bool {
        self.inspector_visible() && self.inspector_identity_current(target)
    }

    pub(super) fn complete_inspector(
        &mut self,
        target: InspectorTarget,
        read: InspectorRead,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        if !self.retired_presentation_enabled() {
            return (false, Vec::new());
        }
        // Cancellation is called before endpoint state changes. It must never
        // dispatch queued work or register a request whose action is discarded.
        if result
            .as_ref()
            .err()
            .is_some_and(|error| error.code.as_deref() == Some("endpoint_cancelled"))
        {
            return (false, Vec::new());
        }
        if self.inspector_identity_current(&target) && !self.inspector_visible() {
            if !self.inspector.queue.contains(&read) {
                self.inspector.queue.push_front(read);
            }
            return (false, Vec::new());
        }
        let mut outcome = ClientShellInput::default();
        if self.inspector_target_current(&target) {
            outcome.repaint = true;
            match (read, result) {
                (
                    InspectorRead::List(path),
                    Ok(ResponseResult::FileList {
                        root,
                        entries,
                        truncated,
                    }),
                ) => {
                    self.inspector.content = None;
                    self.inspector.files_root = clean(&root);
                    self.inspector.directories.insert(path.clone(), entries);
                    self.inspector.expanded.insert(path);
                    self.inspector.message = if truncated {
                        "Listing limited; non-UTF8 names omitted".into()
                    } else {
                        String::new()
                    };
                }
                (
                    InspectorRead::Status,
                    Ok(ResponseResult::GitStatus {
                        root,
                        branch,
                        entries,
                        truncated,
                    }),
                ) => {
                    self.inspector.content = None;
                    self.inspector.git_root = root;
                    self.inspector.branch = clean(&branch);
                    self.inspector.changes = entries;
                    self.inspector.message = if truncated {
                        "Status limited".into()
                    } else if self.inspector.changes.is_empty() {
                        "Working tree clean".into()
                    } else {
                        String::new()
                    };
                }
                (InspectorRead::File { .. }, Ok(ResponseResult::FilePreview { content })) => {
                    self.inspector.content = None;
                    self.inspector.raster = None;
                    match content {
                        FilePreviewContent::Text { text, truncated } => {
                            self.inspector.preview = clean_lines(&text);
                            if truncated {
                                self.inspector.preview.push_str("\n[preview truncated]");
                            }
                            self.inspector.preview_scroll =
                                (self.inspector.preview_line.unwrap_or(1).saturating_sub(1)
                                    as usize)
                                    .min(self.inspector.preview.lines().count().saturating_sub(1));
                        }
                        FilePreviewContent::Image { image } => {
                            self.cache_preview_raster(image, 1, 1)
                        }
                        FilePreviewContent::Pdf { image, page, pages }
                            if page > 0 && page <= pages && pages <= 200 =>
                        {
                            self.cache_preview_raster(image, page, pages)
                        }
                        FilePreviewContent::Unavailable { reason, message } => {
                            self.inspector.preview =
                                format!("{}: {}", clean(&reason), clean(&message))
                        }
                        _ => self.inspector.preview = "Invalid PDF response".into(),
                    }
                }
                (
                    InspectorRead::File { .. },
                    Ok(ResponseResult::FileRead {
                        text,
                        binary,
                        truncated,
                    }),
                ) => {
                    self.inspector.content = None;
                    self.inspector.preview = if binary {
                        "Binary or non-UTF8 file (not previewed)".into()
                    } else {
                        clean_lines(&text)
                    };
                    if truncated {
                        self.inspector.preview.push_str("\n[preview truncated]");
                    }
                    self.inspector.preview_scroll =
                        self.inspector.preview_line.unwrap_or(1).saturating_sub(1) as usize;
                    self.inspector.preview_scroll = self
                        .inspector
                        .preview_scroll
                        .min(self.inspector.preview.lines().count().saturating_sub(1));
                }
                (InspectorRead::Diff { .. }, Ok(ResponseResult::GitDiff { text, truncated })) => {
                    self.inspector.content = None;
                    self.inspector.preview = clean_lines(&text);
                    if truncated {
                        self.inspector.preview.push_str("\n[diff truncated]");
                    }
                }
                (
                    InspectorRead::Log,
                    Ok(ResponseResult::GitLog {
                        root,
                        text,
                        truncated,
                    }),
                ) => {
                    self.inspector.content = None;
                    self.inspector.git_root = root;
                    self.inspector.log = clean_lines(&text);
                    if truncated {
                        self.inspector.log.push_str("\n[history truncated]");
                    }
                }
                (_, Err(error)) => {
                    self.inspector.message = clean(&error.message);
                }
                _ => {
                    self.inspector.message = "Unexpected server read response".into();
                }
            }
        }
        // Even a stale completion releases the single read lane for the new source.
        self.tick_inspector(&mut outcome);
        (outcome.repaint, outcome.actions)
    }

    fn build_inspector_rows(&self) -> Vec<(String, InspectorRead)> {
        let mut rows = Vec::new();
        match self.inspector.view {
            InspectorView::Files => {
                fn append(
                    browser: &Inspector,
                    path: &str,
                    depth: usize,
                    rows: &mut Vec<(String, InspectorRead)>,
                ) {
                    if depth > MAX_EXPANDED {
                        return;
                    }
                    if let Some(entries) = browser.directories.get(path) {
                        for entry in entries {
                            let name = entry.path.rsplit('/').next().unwrap_or(&entry.path);
                            let marker = if entry.symlink {
                                "@"
                            } else if entry.directory {
                                if browser.expanded.contains(&entry.path) {
                                    "v"
                                } else {
                                    ">"
                                }
                            } else {
                                " "
                            };
                            let read = if entry.directory && !entry.symlink {
                                InspectorRead::List(entry.path.clone())
                            } else {
                                InspectorRead::File {
                                    root: browser
                                        .source
                                        .as_ref()
                                        .map(|source| source.cwd.clone())
                                        .unwrap_or_default(),
                                    path: entry.path.clone(),
                                }
                            };
                            rows.push((
                                format!("{}{} {}", " ".repeat(depth.min(8)), marker, clean(name)),
                                read,
                            ));
                            if entry.directory && browser.expanded.contains(&entry.path) {
                                append(browser, &entry.path, depth + 1, rows);
                            }
                        }
                    }
                }
                append(&self.inspector, "", 0, &mut rows);
            }
            InspectorView::Git => {
                for entry in &self.inspector.changes {
                    let label = match &entry.original_path {
                        Some(old) => format!("{} -> {}", clean(old), clean(&entry.path)),
                        None => clean(&entry.path),
                    };
                    if entry.index == "?" {
                        rows.push((
                            format!("?? {label} (preview)"),
                            InspectorRead::File {
                                root: self.inspector.git_root.clone(),
                                path: entry.path.clone(),
                            },
                        ));
                    } else {
                        if entry.index != " " {
                            rows.push((
                                format!("S:{} {label}", clean(&entry.index)),
                                InspectorRead::Diff {
                                    path: entry.path.clone(),
                                    original_path: entry.original_path.clone(),
                                    staged: true,
                                },
                            ));
                        }
                        if entry.worktree != " " {
                            rows.push((
                                format!("U:{} {label}", clean(&entry.worktree)),
                                InspectorRead::Diff {
                                    path: entry.path.clone(),
                                    original_path: entry.original_path.clone(),
                                    staged: false,
                                },
                            ));
                        }
                    }
                }
            }
            InspectorView::Log | InspectorView::Tasks => {}
        }
        rows
    }

    fn preview_bounds(&self) -> (u32, u32) {
        let Some((cols, rows)) = self.last_composed_size else {
            return (40, 80);
        };
        let area = self.layout(cols, rows).inspector;
        let list = Self::inspector_list_area(area);
        (
            u32::from(area.width.saturating_sub(2)).clamp(1, 160),
            u32::from(area.bottom().saturating_sub(list.bottom() + 1))
                .saturating_mul(2)
                .clamp(1, 160),
        )
    }

    fn cache_preview_raster(&mut self, image: PreviewRaster, page: u32, pages: u32) {
        self.inspector.content = None;
        match validate_raster(image) {
            Ok(raster) => {
                self.inspector.raster = Some(raster);
                self.inspector.preview.clear();
                self.inspector.page = page;
                self.inspector.pages = pages;
            }
            Err(error) => self.inspector.preview = error.into(),
        }
    }

    pub(super) fn inspector_list_area(area: Rect) -> Rect {
        Rect::new(
            area.x + 1,
            area.y + 5,
            area.width.saturating_sub(2),
            area.height.saturating_sub(8) / 2,
        )
    }

    fn prepare_inspector_content(&mut self) {
        if self.inspector.content.is_some() {
            return;
        }
        use unicode_width::UnicodeWidthStr;
        let list = self.build_inspector_rows();
        let list_columns = list
            .iter()
            .map(|(label, _)| UnicodeWidthStr::width(label.as_str()))
            .max()
            .unwrap_or(0);
        let text = if self.inspector.view == InspectorView::Log {
            &self.inspector.log
        } else {
            &self.inspector.preview
        };
        let mut preview_rows = 0;
        let mut preview_columns = 0;
        if self.inspector.raster.is_none() {
            for line in text.lines() {
                preview_rows += 1;
                preview_columns = preview_columns.max(UnicodeWidthStr::width(reference_line(
                    line,
                    self.inspector.preview_column,
                )));
            }
        }
        self.inspector.content = Some(InspectorContent {
            list,
            list_columns,
            preview_rows,
            preview_columns,
        });
        #[cfg(test)]
        {
            self.inspector.content_builds += 1;
        }
    }

    fn inspector_rows(&self) -> &[(String, InspectorRead)] {
        self.inspector
            .content
            .as_ref()
            .map_or(&[], |content| &content.list)
    }

    fn inspector_regions(&self, area: Rect) -> (InspectorRegion, InspectorRegion) {
        if self.inspector.view == InspectorView::Tasks {
            return self.task_regions(
                area,
                self.inspector.list_scroll,
                self.inspector.list_horizontal_scroll,
                self.inspector.preview_scroll,
                self.inspector.preview_horizontal_scroll,
            );
        }
        let content = self.inspector.content.as_ref();
        let list = Self::inspector_list_area(area);
        let list_region = InspectorRegion::new(
            list,
            content.map_or(0, |content| content.list.len()),
            content.map_or(0, |content| content.list_columns),
            self.inspector.list_scroll,
            self.inspector.list_horizontal_scroll,
        );
        let preview = if self.inspector.view == InspectorView::Log {
            Rect::new(
                area.x + 1,
                area.y + 5,
                area.width.saturating_sub(2),
                area.height.saturating_sub(5),
            )
        } else {
            Rect::new(
                area.x + 1,
                list.bottom() + 1,
                area.width.saturating_sub(2),
                area.bottom().saturating_sub(list.bottom() + 1),
            )
        };
        let preview_region = InspectorRegion::new(
            preview,
            content.map_or(0, |content| content.preview_rows),
            content.map_or(0, |content| content.preview_columns),
            self.inspector.preview_scroll,
            self.inspector.preview_horizontal_scroll,
        );
        (list_region, preview_region)
    }

    pub(super) fn cancel_inspector_drag(&mut self) {
        self.inspector.drag = None;
    }

    pub(super) fn clamp_inspector_scroll(&mut self) {
        if !self.retired_presentation_enabled() {
            return;
        }
        let Some((cols, rows)) = self.last_composed_size else {
            return;
        };
        let area = self.layout(cols, rows).inspector;
        if area.is_empty() {
            self.inspector.drag = None;
            return;
        }
        if self.inspector.view != InspectorView::Tasks {
            self.prepare_inspector_content();
        }
        let (list, preview) = self.inspector_regions(area);
        self.inspector.list_scroll = list.top;
        self.inspector.list_horizontal_scroll = list.left;
        self.inspector.preview_scroll = preview.top;
        self.inspector.preview_horizontal_scroll = preview.left;
    }

    fn set_inspector_scroll(&mut self, list: bool, horizontal: bool, offset: usize) {
        let scroll = match (list, horizontal) {
            (true, true) => &mut self.inspector.list_horizontal_scroll,
            (true, false) => &mut self.inspector.list_scroll,
            (false, true) => &mut self.inspector.preview_horizontal_scroll,
            (false, false) => &mut self.inspector.preview_scroll,
        };
        *scroll = offset;
    }

    fn drag_inspector(&mut self, mouse: MouseEvent, outcome: &mut ClientShellInput) {
        let Some(drag) = self.inspector.drag else {
            return;
        };
        let Some((cols, rows)) = self.last_composed_size else {
            return;
        };
        let area = self.layout(cols, rows).inspector;
        if area.is_empty() || !self.inspector.open {
            self.inspector.drag = None;
            return;
        }
        match drag {
            InspectorDrag::Width => {
                let available = self.layout(cols, rows).pane_surface.width + area.width;
                let width = area
                    .right()
                    .saturating_sub(mouse.column)
                    .clamp(MIN_WIDTH, available - MIN_TERMINAL_WIDTH);
                if width != area.width {
                    self.inspector.preferred_width = Some(width);
                    self.invalidate_pane_surface();
                    self.clamp_inspector_scroll();
                    outcome.resize = true;
                    outcome.repaint = true;
                }
            }
            InspectorDrag::Scrollbar {
                list,
                horizontal,
                grab_offset,
                last_position,
            } => {
                if self.inspector.view != InspectorView::Tasks
                    && !self
                        .inspector
                        .source
                        .as_ref()
                        .is_some_and(|source| self.inspector_source_current(source))
                {
                    self.inspector.drag = None;
                    return;
                }
                let (list_region, preview_region) = self.inspector_regions(area);
                let region = if list { list_region } else { preview_region };
                let position = if horizontal { mouse.column } else { mouse.row };
                let track = region.track(horizontal);
                if track.is_empty() {
                    self.inspector.drag = None;
                } else if position != last_position {
                    let metrics = region.metrics(horizontal);
                    let bottom_offset = crate::ui::scrollbar_offset_from_drag_row(
                        metrics,
                        track,
                        position,
                        grab_offset,
                    );
                    self.set_inspector_scroll(
                        list,
                        horizontal,
                        metrics.max_offset_from_bottom.saturating_sub(bottom_offset),
                    );
                    self.inspector.drag = Some(InspectorDrag::Scrollbar {
                        list,
                        horizontal,
                        grab_offset,
                        last_position: position,
                    });
                    outcome.repaint = true;
                }
            }
        }
    }

    pub(super) fn render_inspector(&self, buffer: &mut Buffer, area: Rect) {
        if !self.retired_presentation_enabled() {
            return;
        }
        if area.is_empty() {
            return;
        }
        let style = Style::default()
            .fg(self.config.palette.text)
            .bg(self.config.palette.panel_bg);
        buffer.set_style(area, style);
        for y in area.y..area.bottom() {
            buffer[(area.x, y)]
                .set_symbol("│")
                .set_style(style.fg(self.config.palette.overlay0));
        }
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
        put(buffer, area.y, "Inspector");
        render::put_text(
            buffer,
            area.right().saturating_sub(14),
            area.y,
            9,
            "[refresh]",
            style,
        );
        render::put_text(
            buffer,
            area.right().saturating_sub(4),
            area.y,
            3,
            "[x]",
            style,
        );
        put(
            buffer,
            area.y + 1,
            match (self.inspector.view, area.width >= 32) {
                (InspectorView::Files, true) => "[Files] Git  Commits   Tasks",
                (InspectorView::Git, true) => " Files [Git] Commits   Tasks",
                (InspectorView::Log, true) => " Files  Git [Commits]  Tasks",
                (InspectorView::Tasks, true) => " Files  Git  Commits  [Tasks]",
                (InspectorView::Files, false) => "[Files] Git   Log   Tasks",
                (InspectorView::Git, false) => " Files [Git]  Log   Tasks",
                (InspectorView::Log, false) => " Files  Git  [Log]  Tasks",
                (InspectorView::Tasks, false) => " Files  Git   Log  [Tasks]",
            },
        );
        if self.inspector.view == InspectorView::Tasks {
            self.render_tasks(buffer, area);
            return;
        }
        if !self
            .inspector
            .source
            .as_ref()
            .is_some_and(|source| self.inspector_source_current(source))
        {
            put(buffer, area.y + 2, "Waiting for focused pane cwd...");
            return;
        }
        let source = self
            .inspector
            .source
            .as_ref()
            .map(|source| source.cwd.as_str())
            .unwrap_or("");
        put(
            buffer,
            area.y + 2,
            &format!(
                "Files: {}",
                if self.inspector.files_root.is_empty() {
                    clean(source)
                } else {
                    self.inspector.files_root.clone()
                }
            ),
        );
        put(
            buffer,
            area.y + 3,
            &format!(
                "Git: {}",
                if self.inspector.git_root.is_empty() {
                    "(load Git / Commits)".into()
                } else {
                    clean(&self.inspector.git_root)
                }
            ),
        );
        put(
            buffer,
            area.y + 4,
            if self.inspector.message.is_empty() {
                &self.inspector.branch
            } else {
                &self.inspector.message
            },
        );
        let list = Self::inspector_list_area(area);
        let (list_region, preview_region) = self.inspector_regions(area);
        let paint_line = |buffer: &mut Buffer, region: InspectorRegion, row, line: &str| {
            render::put_text(
                buffer,
                region.content.x,
                row,
                region.content.width,
                &crop_cells(line, region.left, region.content.width),
                style,
            );
        };
        if self.inspector.view == InspectorView::Log {
            for (i, line) in self
                .inspector
                .log
                .lines()
                .skip(preview_region.top)
                .take(usize::from(preview_region.content.height))
                .enumerate()
            {
                paint_line(
                    buffer,
                    preview_region,
                    preview_region.content.y + i as u16,
                    reference_line(line, self.inspector.preview_column),
                );
            }
            preview_region.paint(buffer, &self.config.palette);
            return;
        }
        for (i, (label, _)) in self
            .inspector_rows()
            .iter()
            .skip(list_region.top)
            .take(usize::from(list_region.content.height))
            .enumerate()
        {
            paint_line(buffer, list_region, list_region.content.y + i as u16, label);
        }
        list_region.paint(buffer, &self.config.palette);
        let preview_y = list.bottom();
        put(
            buffer,
            preview_y,
            &format!("-- {}", clean(&self.inspector.selection)),
        );
        if self.inspector.pages > 1 {
            render::put_text(
                buffer,
                area.right().saturating_sub(18),
                preview_y,
                3,
                "[-]",
                style,
            );
            render::put_text(
                buffer,
                area.right().saturating_sub(14),
                preview_y,
                9,
                &format!("{}/{}", self.inspector.page, self.inspector.pages),
                style,
            );
            render::put_text(
                buffer,
                area.right().saturating_sub(4),
                preview_y,
                3,
                "[+]",
                style,
            );
        }
        if let Some(raster) = &self.inspector.raster {
            let x = area.x + 1;
            let y = preview_y + 1;
            let color = |col: u32, row: u32| {
                let index = (row as usize * raster.width as usize + col as usize) * 3;
                ratatui::style::Color::Rgb(
                    raster.rgb[index],
                    raster.rgb[index + 1],
                    raster.rgb[index + 2],
                )
            };
            for row in 0..raster
                .height
                .div_ceil(2)
                .min(u32::from(area.bottom().saturating_sub(y)))
            {
                for col in 0..raster.width.min(u32::from(area.width.saturating_sub(2))) {
                    let top = color(col, row * 2);
                    let bottom = if row * 2 + 1 < raster.height {
                        color(col, row * 2 + 1)
                    } else {
                        self.config.palette.panel_bg
                    };
                    if let Some(cell) = buffer.cell_mut((x + col as u16, y + row as u16)) {
                        cell.set_char('▀').set_style(style.fg(top).bg(bottom));
                    }
                }
            }
            return;
        }
        for (i, line) in self
            .inspector
            .preview
            .lines()
            .skip(preview_region.top)
            .take(usize::from(preview_region.content.height))
            .enumerate()
        {
            paint_line(
                buffer,
                preview_region,
                preview_region.content.y + i as u16,
                reference_line(line, self.inspector.preview_column),
            );
        }
        preview_region.paint(buffer, &self.config.palette);
    }

    pub(super) fn handle_inspector_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !self.retired_presentation_enabled() {
            return false;
        }
        if self.inspector.pointer_capture {
            if matches!(
                mouse.kind,
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
            ) {
                self.drag_inspector(mouse, outcome);
            }
            if matches!(mouse.kind, MouseEventKind::Up(_)) {
                self.inspector.pointer_capture = false;
                self.inspector.drag = None;
                self.tick_inspector(outcome);
                return true;
            }
            if !matches!(mouse.kind, MouseEventKind::Down(_)) {
                // Capture is handled BEFORE other chrome/PTY routing. Retain a
                // release sink even when close/source loss cancels the drag.
                return true;
            }
            self.inspector.drag = None;
        }
        if self.overlay.is_some()
            || self.popup_pending
            || self.popup_terminal_id.is_some()
            || self.hits.popup.is_some()
            || !self.config.mouse_capture
            || self.pane_mouse_gesture.is_some()
            || self.chrome_drag.is_some()
            || self.workspace_press.is_some()
            || self.tab_press.is_some()
            || self
                .word_selection_gesture
                .as_ref()
                .is_some_and(|gesture| !gesture.released)
            || self
                .selection
                .as_ref()
                .is_some_and(|selection| selection.is_in_progress())
        {
            return false;
        }
        let Some((cols, rows)) = self.last_composed_size else {
            return false;
        };
        let area = self.layout(cols, rows).inspector;
        if area.is_empty() || !contains(area, (mouse.column, mouse.row)) {
            return false;
        }
        if matches!(mouse.kind, MouseEventKind::Down(_)) {
            self.inspector.pointer_capture = true;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            self.pending_file_open = None;
            if mouse.column == area.x {
                self.inspector.drag = Some(InspectorDrag::Width);
                return true;
            }
            if mouse.row == area.y && mouse.column >= area.right().saturating_sub(4) {
                self.toggle_inspector(outcome);
                return true;
            }
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) && mouse.row == area.y + 1 {
            let column = mouse.column - area.x;
            self.inspector.view = if column < 8 {
                InspectorView::Files
            } else if column < 14 {
                InspectorView::Git
            } else if column < if area.width >= 32 { 23 } else { 20 } {
                InspectorView::Log
            } else {
                InspectorView::Tasks
            };
            self.reset_inspector_data(self.inspector_source());
            self.invalidate_tasks();
            self.tick_inspector(outcome);
            outcome.repaint = true;
            return true;
        }
        // Consume all pointer events in the panel, including wheel/release/drag;
        // none are PTY input. There is deliberately no global keyboard shortcut.
        if self.inspector.view != InspectorView::Tasks
            && !self
                .inspector
                .source
                .as_ref()
                .is_some_and(|source| self.inspector_source_current(source))
        {
            self.tick_inspector(outcome);
            return true;
        }
        self.clamp_inspector_scroll();
        let list = Self::inspector_list_area(area);
        let (list_region, preview_region) = self.inspector_regions(area);
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            for (is_list, region) in [(true, list_region), (false, preview_region)] {
                if is_list && self.inspector.view == InspectorView::Log {
                    continue;
                }
                for horizontal in [false, true] {
                    let painted_track = if horizontal {
                        region.horizontal
                    } else {
                        region.vertical
                    };
                    if painted_track.is_empty()
                        || !contains(painted_track, (mouse.column, mouse.row))
                    {
                        continue;
                    }
                    let position = if horizontal { mouse.column } else { mouse.row };
                    let metrics = region.metrics(horizontal);
                    let track = region.track(horizontal);
                    let grab_offset = if let Some(grab) =
                        crate::ui::scrollbar_thumb_grab_offset(metrics, track, position)
                    {
                        grab
                    } else {
                        let bottom_offset =
                            crate::ui::scrollbar_offset_from_row(metrics, track, position);
                        self.set_inspector_scroll(
                            is_list,
                            horizontal,
                            metrics.max_offset_from_bottom.saturating_sub(bottom_offset),
                        );
                        let updated = crate::pane::ScrollMetrics {
                            offset_from_bottom: bottom_offset,
                            ..metrics
                        };
                        crate::ui::scrollbar_thumb_grab_offset(updated, track, position)
                            .unwrap_or(0)
                    };
                    self.inspector.drag = Some(InspectorDrag::Scrollbar {
                        list: is_list,
                        horizontal,
                        grab_offset,
                        last_position: position,
                    });
                    outcome.repaint = true;
                    return true;
                }
            }
        }
        if self.inspector.view == InspectorView::Tasks {
            self.handle_tasks_mouse(mouse, area, list_region, preview_region, outcome);
            self.clamp_inspector_scroll();
            self.tick_tasks(outcome);
            outcome.repaint = true;
            return true;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if mouse.row == list.bottom()
                    && self.inspector.pages > 1
                    && ((area.right().saturating_sub(18)..area.right().saturating_sub(15))
                        .contains(&mouse.column)
                        || (area.right().saturating_sub(4)..area.right().saturating_sub(1))
                            .contains(&mouse.column)) =>
            {
                let next = if mouse.column >= area.right().saturating_sub(4) {
                    self.inspector
                        .page
                        .saturating_add(1)
                        .min(self.inspector.pages)
                } else {
                    self.inspector.page.saturating_sub(1).max(1)
                };
                if next != self.inspector.page {
                    if let Some((root, path)) = self.inspector.preview_file.clone() {
                        self.inspector.page = next;
                        self.inspector.generation = self.inspector.generation.wrapping_add(1);
                        self.inspector.queue.clear();
                        self.inspector
                            .queue
                            .push_back(InspectorRead::File { root, path });
                        self.inspector.raster = None;
                        self.inspector.preview = "Loading page...".into();
                        self.inspector.content = None;
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) if mouse.row == area.y => {
                self.reset_inspector_data(self.inspector_source());
            }
            MouseEventKind::Down(MouseButton::Left)
                if contains(list_region.content, (mouse.column, mouse.row))
                    && self.inspector.view != InspectorView::Log =>
            {
                if let Some((label, read)) = self
                    .inspector_rows()
                    .get(list_region.top + usize::from(mouse.row - list_region.content.y))
                    .cloned()
                {
                    self.inspector.generation = self.inspector.generation.wrapping_add(1);
                    self.inspector.queue.clear();
                    self.inspector.content = None;
                    match &read {
                        InspectorRead::List(path) if self.inspector.expanded.remove(path) => {
                            let prefix = format!("{path}/");
                            self.inspector
                                .directories
                                .retain(|key, _| key != path && !key.starts_with(&prefix));
                            self.inspector
                                .expanded
                                .retain(|key| !key.starts_with(&prefix));
                        }
                        InspectorRead::List(_) if self.inspector.expanded.len() >= MAX_EXPANDED => {
                            self.inspector.message =
                                "Expansion limit reached; collapse or refresh".into();
                        }
                        _ => {
                            self.inspector.selection = label;
                            self.inspector.preview.clear();
                            self.inspector.raster = None;
                            self.inspector.page = 1;
                            self.inspector.pages = 1;
                            self.inspector.preview_scroll = 0;
                            self.inspector.preview_horizontal_scroll = 0;
                            self.inspector.preview_line = None;
                            self.inspector.preview_column = None;
                            self.inspector.queue.push_back(read);
                        }
                    }
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let scroll_list = contains(list, (mouse.column, mouse.row))
                    && self.inspector.view != InspectorView::Log;
                let region = if scroll_list {
                    list_region
                } else {
                    preview_region
                };
                let limit = region.metrics(false).max_offset_from_bottom;
                let scroll = if scroll_list {
                    &mut self.inspector.list_scroll
                } else {
                    &mut self.inspector.preview_scroll
                };
                *scroll = if mouse.kind == MouseEventKind::ScrollDown {
                    scroll.saturating_add(3).min(limit)
                } else {
                    scroll.saturating_sub(3)
                };
            }
            _ => {}
        }
        outcome.repaint = true;
        self.tick_inspector(outcome);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ClientShellState {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        state.retired_presentation_fixture = true;
        state.set_snapshot(Box::new(crate::client::shell::tests::snapshot()));
        state.set_endpoint_methods(Some(
            [
                "file.list",
                "file.read",
                "file.preview",
                "git.status",
                "git.diff",
                "git.log",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        ));
        state.last_composed_size = Some((180, 40));
        state
    }
    fn open(state: &mut ClientShellState) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        state.toggle_inspector(&mut outcome);
        outcome
    }
    fn target(state: &ClientShellState) -> InspectorTarget {
        InspectorTarget {
            source: state.inspector_source().unwrap(),
            generation: state.inspector.generation,
        }
    }
    fn list_result() -> ResponseResult {
        ResponseResult::FileList {
            root: "/repo".into(),
            entries: vec![FileEntry {
                path: "file".into(),
                directory: false,
                symlink: false,
            }],
            truncated: false,
        }
    }
    fn mouse(
        state: &mut ClientShellState,
        kind: MouseEventKind,
        col: u16,
        row: u16,
    ) -> ClientShellInput {
        state.handle_raw_events(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
            kind,
            column: col,
            row,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })])
    }

    fn overflowing(state: &mut ClientShellState) {
        open(state);
        state.pending_requests.clear();
        state.inspector.queue.clear();
        state.inspector.directories.insert(
            String::new(),
            (0..100)
                .map(|i| FileEntry {
                    path: format!("{i:03}-{}", "long-path-".repeat(12)),
                    directory: false,
                    symlink: false,
                })
                .collect(),
        );
        state.inspector.preview = (0..100)
            .map(|i| format!("{i:03} {}\n", "界e\u{301}👩‍💻".repeat(20)))
            .collect();
        state.inspector.content = None;
        state.clamp_inspector_scroll();
    }

    #[test]
    fn inspector_shallow_long_filename_list_remains_rendered_and_selectable() {
        let mut state = state();
        state.last_composed_size = Some((180, 10));
        open(&mut state);
        state.pending_requests.clear();
        let paths = [
            format!("first-{}", "long".repeat(30)),
            format!("second-{}", "long".repeat(30)),
        ];
        state.complete_inspector(
            target(&state),
            InspectorRead::List(String::new()),
            Ok(ResponseResult::FileList {
                root: "/repo".into(),
                entries: paths
                    .iter()
                    .map(|path| FileEntry {
                        path: path.clone(),
                        directory: false,
                        symlink: false,
                    })
                    .collect(),
                truncated: false,
            }),
        );
        let area = state.layout(180, 10).inspector;
        let (list, _) = state.inspector_regions(area);
        assert_eq!(list.content.height, 1);
        assert!(list.content.width > 0 && list.horizontal.is_empty());
        assert_eq!(list.vertical.height, 1);
        for (index, path) in paths.iter().enumerate() {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 180, 10));
            state.render_inspector(&mut buffer, area);
            let visible: String = (list.content.x..list.content.right())
                .map(|x| buffer[(x, list.content.y)].symbol())
                .collect();
            assert!(visible.starts_with(&format!("  {}", &path[..6])));
            state.pending_requests.clear();
            let down = mouse(
                &mut state,
                MouseEventKind::Down(MouseButton::Left),
                list.content.x + 2,
                list.content.y,
            );
            assert!(down.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::FilePreview(params) if params.path == *path))));
            assert!(state.inspector.selection.contains(path));
            mouse(
                &mut state,
                MouseEventKind::Up(MouseButton::Left),
                list.content.x + 2,
                list.content.y,
            );
            if index == 0 {
                mouse(
                    &mut state,
                    MouseEventKind::ScrollDown,
                    list.content.x,
                    list.content.y,
                );
                assert_eq!(state.inspector.list_scroll, 1);
            }
        }

        // Even a one-cell viewport retains a paintable content cell, not bars.
        let cell = Rect::new(0, 0, 1, 1);
        let edge = InspectorRegion::new(cell, 100, 100, usize::MAX, usize::MAX);
        assert_eq!(edge.content, cell);
        assert!(edge.vertical.is_empty() && edge.horizontal.is_empty());
        assert_eq!((edge.top, edge.left), (99, 99));
        let mut buffer = Buffer::empty(cell);
        render::put_text(
            &mut buffer,
            0,
            0,
            1,
            &crop_cells("xy", 1, 1),
            Style::default(),
        );
        edge.paint(&mut buffer, &state.config.palette);
        assert_eq!(buffer[(0, 0)].symbol(), "y");
    }

    #[test]
    fn inspector_unchanged_ticks_compositions_and_render_reuse_prepared_content() {
        for view in [InspectorView::Files, InspectorView::Git, InspectorView::Log] {
            let mut state = state();
            overflowing(&mut state);
            state.inspector.view = view;
            state.inspector.log = "long history line ".repeat(100) + &"\nentry".repeat(100);
            state.inspector.content = None;
            state.clamp_inspector_scroll();
            let builds = state.inspector.content_builds;
            let rows = state.inspector_rows().as_ptr();
            for _ in 0..20 {
                let mut outcome = ClientShellInput::default();
                state.tick_inspector(&mut outcome);
                state.compose(180, 40);
                let mut buffer = Buffer::empty(Rect::new(0, 0, 180, 40));
                state.render_inspector(&mut buffer, state.layout(180, 40).inspector);
                assert_eq!(state.inspector.content_builds, builds);
                assert_eq!(state.inspector_rows().as_ptr(), rows);
                assert!(outcome.actions.is_empty());
            }
            // Geometry and offsets only use cached scalars, even on resize.
            state.compose(121, 30);
            state.compose(240, 60);
            assert_eq!(state.inspector.content_builds, builds);
        }
    }

    #[test]
    fn inspector_content_cache_invalidates_for_responses_tree_source_view_and_column() {
        fn complete(state: &mut ClientShellState, read: InspectorRead, result: ResponseResult) {
            state.pending_requests.clear();
            state.inspector.queue.clear();
            let builds = state.inspector.content_builds;
            state.complete_inspector(target(state), read, Ok(result));
            assert_eq!(state.inspector.content_builds, builds + 1);
        }
        let mut state = state();
        open(&mut state);
        complete(
            &mut state,
            InspectorRead::List(String::new()),
            ResponseResult::FileList {
                root: "/repo".into(),
                entries: vec![FileEntry {
                    path: "dir".into(),
                    directory: true,
                    symlink: false,
                }],
                truncated: false,
            },
        );
        let area = state.layout(180, 40).inspector;
        let (list, _) = state.inspector_regions(area);
        mouse(
            &mut state,
            MouseEventKind::Down(MouseButton::Left),
            list.content.x,
            list.content.y,
        );
        mouse(
            &mut state,
            MouseEventKind::Up(MouseButton::Left),
            list.content.x,
            list.content.y,
        );
        complete(
            &mut state,
            InspectorRead::List("dir".into()),
            ResponseResult::FileList {
                root: "/repo".into(),
                entries: vec![FileEntry {
                    path: format!("dir/{}", "child".repeat(30)),
                    directory: false,
                    symlink: false,
                }],
                truncated: false,
            },
        );
        assert_eq!(state.inspector_rows().len(), 2);
        assert!(state.inspector_regions(area).0.columns > 100);
        state.inspector.list_scroll = usize::MAX;
        state.inspector.list_horizontal_scroll = usize::MAX;
        state.clamp_inspector_scroll();
        let builds = state.inspector.content_builds;
        mouse(
            &mut state,
            MouseEventKind::Down(MouseButton::Left),
            list.content.x,
            list.content.y,
        );
        mouse(
            &mut state,
            MouseEventKind::Up(MouseButton::Left),
            list.content.x,
            list.content.y,
        );
        assert_eq!(state.inspector.content_builds, builds + 1);
        assert_eq!(state.inspector_rows().len(), 1);
        assert_eq!(
            (
                state.inspector.list_scroll,
                state.inspector.list_horizontal_scroll
            ),
            (0, 0)
        );

        let read = InspectorRead::File {
            root: "/repo".into(),
            path: "a".into(),
        };
        for column in [Some(2), Some(200)] {
            let source = state.inspector_source().unwrap();
            state.open_file_reference(
                source,
                crate::api::schema::FileReference {
                    path: "a".into(),
                    line: None,
                    column,
                },
                &mut ClientShellInput::default(),
            );
            complete(
                &mut state,
                read.clone(),
                ResponseResult::FilePreview {
                    content: FilePreviewContent::Text {
                        text: "界".to_owned() + &"x".repeat(100) + "\nshort",
                        truncated: false,
                    },
                },
            );
            let (_, preview) = state.inspector_regions(area);
            assert_eq!(preview.columns, if column == Some(2) { 100 } else { 0 });
            state.inspector.preview_horizontal_scroll = usize::MAX;
            state.clamp_inspector_scroll();
            assert_eq!(
                state.inspector.preview_horizontal_scroll,
                preview
                    .columns
                    .saturating_sub(usize::from(preview.content.width))
            );
        }
        state.inspector.preview_scroll = usize::MAX;
        complete(
            &mut state,
            read,
            ResponseResult::FileRead {
                text: "short".into(),
                binary: false,
                truncated: false,
            },
        );
        assert_eq!(
            (
                state.inspector.preview_scroll,
                state.inspector.preview_horizontal_scroll
            ),
            (0, 0)
        );

        for (column, read, result, expected_rows) in [
            (
                10,
                InspectorRead::Status,
                ResponseResult::GitStatus {
                    root: "/repo".into(),
                    branch: "main".into(),
                    entries: vec![GitFileEntry {
                        path: "changed".into(),
                        original_path: None,
                        index: "M".into(),
                        worktree: " ".into(),
                    }],
                    truncated: false,
                },
                1,
            ),
            (
                20,
                InspectorRead::Log,
                ResponseResult::GitLog {
                    root: "/repo".into(),
                    text: "history\nentry".into(),
                    truncated: false,
                },
                2,
            ),
        ] {
            mouse(
                &mut state,
                MouseEventKind::Down(MouseButton::Left),
                area.x + column,
                area.y + 1,
            );
            mouse(
                &mut state,
                MouseEventKind::Up(MouseButton::Left),
                area.x + column,
                area.y + 1,
            );
            complete(&mut state, read, result);
            let content = state.inspector.content.as_ref().unwrap();
            assert_eq!(
                if column == 10 {
                    content.list.len()
                } else {
                    content.preview_rows
                },
                expected_rows
            );
        }
        complete(
            &mut state,
            InspectorRead::Diff {
                path: "changed".into(),
                original_path: None,
                staged: true,
            },
            ResponseResult::GitDiff {
                text: "diff\nline".into(),
                truncated: false,
            },
        );
        let builds = state.inspector.content_builds;
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.panes[0].foreground_cwd = Some("/new-source".into());
        state.set_snapshot(Box::new(snapshot));
        state.tick_inspector(&mut ClientShellInput::default());
        state.compose(180, 40);
        assert_eq!(state.inspector.content_builds, builds + 1);
        let content = state.inspector.content.as_ref().unwrap();
        assert!(content.list.is_empty());
        assert_eq!((content.preview_rows, content.preview_columns), (0, 0));
        assert_eq!(
            (
                state.inspector.preview_scroll,
                state.inspector.preview_horizontal_scroll
            ),
            (0, 0)
        );
    }

    #[test]
    fn inspector_scrollbar_paint_hit_track_click_and_drag_agree_on_both_axes() {
        for is_list in [true, false] {
            for horizontal in [false, true] {
                let mut state = state();
                overflowing(&mut state);
                let area = state.layout(180, 40).inspector;
                let (list, preview) = state.inspector_regions(area);
                let region = if is_list { list } else { preview };
                let track = if horizontal {
                    region.horizontal
                } else {
                    region.vertical
                };
                let metrics = region.metrics(horizontal);
                let thumb = crate::ui::scrollbar_thumb(metrics, region.track(horizontal)).unwrap();
                assert_eq!(thumb.top, if horizontal { track.x } else { track.y });
                let mut buffer = Buffer::empty(Rect::new(0, 0, 180, 40));
                state.render_inspector(&mut buffer, area);
                for position in 0..if horizontal {
                    track.width
                } else {
                    track.height
                } {
                    let (x, y) = if horizontal {
                        (track.x + position, track.y)
                    } else {
                        (track.x, track.y + position)
                    };
                    let symbol = if position < thumb.len {
                        if horizontal {
                            "━"
                        } else {
                            "▐"
                        }
                    } else if horizontal {
                        "─"
                    } else {
                        "▕"
                    };
                    assert_eq!(buffer[(x, y)].symbol(), symbol);
                }
                let (x, y) = if horizontal {
                    (thumb.top, track.y)
                } else {
                    (track.x, thumb.top)
                };
                let down = mouse(&mut state, MouseEventKind::Down(MouseButton::Left), x, y);
                assert!(down.requests.is_empty() && down.actions.is_empty());
                assert!(
                    matches!(state.inspector.drag, Some(InspectorDrag::Scrollbar { list, horizontal: axis, .. }) if list == is_list && axis == horizontal)
                );
                // A grab and a stationary move must not round-trip/jump offsets.
                mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), x, y);
                let (l, p) = state.inspector_regions(area);
                assert_eq!(
                    if is_list { l } else { p }
                        .metrics(horizontal)
                        .offset_from_bottom,
                    metrics.max_offset_from_bottom
                );
                let (end_x, end_y) = if horizontal {
                    (179, track.y)
                } else {
                    (track.x, 39)
                };
                assert!(mouse(
                    &mut state,
                    MouseEventKind::Drag(MouseButton::Left),
                    end_x,
                    end_y
                )
                .requests
                .is_empty());
                let (l, p) = state.inspector_regions(area);
                let end = if is_list { l } else { p };
                assert_eq!(end.metrics(horizontal).offset_from_bottom, 0);
                let end_thumb =
                    crate::ui::scrollbar_thumb(end.metrics(horizontal), end.track(horizontal))
                        .unwrap();
                assert_eq!(
                    end_thumb.top + end_thumb.len,
                    if horizontal {
                        track.right()
                    } else {
                        track.bottom()
                    }
                );
                // Cross panel boundaries in the opposite direction and release
                // over a terminal. No PTY action or selection may be started.
                assert!(
                    mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 0, 0)
                        .requests
                        .is_empty()
                );
                let release = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 30, 10);
                assert!(release.requests.is_empty() && release.actions.is_empty());
                assert!(!state.inspector.pointer_capture && state.inspector.drag.is_none());
                assert!(state.selection.is_none() && state.pane_mouse_gesture.is_none());
                // Track click maps through exactly the same thumb geometry.
                let position = if horizontal {
                    track.right() - 1
                } else {
                    track.bottom() - 1
                };
                let (x, y) = if horizontal {
                    (position, track.y)
                } else {
                    (track.x, position)
                };
                let (l, p) = state.inspector_regions(area);
                let current = if is_list { l } else { p };
                let expected = crate::ui::scrollbar_offset_from_row(
                    current.metrics(horizontal),
                    current.track(horizontal),
                    position,
                );
                mouse(&mut state, MouseEventKind::Down(MouseButton::Left), x, y);
                let (l, p) = state.inspector_regions(area);
                assert_eq!(
                    if is_list { l } else { p }
                        .metrics(horizontal)
                        .offset_from_bottom,
                    expected
                );
                mouse(&mut state, MouseEventKind::Up(MouseButton::Left), x, y);
                assert!(state
                    .handle_input_bytes(b"normal typing")
                    .requests
                    .iter()
                    .any(|request| matches!(request, ClientMessage::ClientShellPaneInput { .. })));
            }
        }
    }

    #[test]
    fn inspector_thumb_nonzero_grab_offset_does_not_jump() {
        let mut state = state();
        open(&mut state);
        state.inspector.preview = "line\n".repeat(24);
        state.inspector.content = None;
        state.clamp_inspector_scroll();
        let area = state.layout(180, 40).inspector;
        let (_, region) = state.inspector_regions(area);
        let thumb = crate::ui::scrollbar_thumb(region.metrics(false), region.track(false)).unwrap();
        assert!(thumb.len > 1);
        mouse(
            &mut state,
            MouseEventKind::Down(MouseButton::Left),
            region.vertical.x,
            thumb.top + 1,
        );
        assert!(matches!(
            state.inspector.drag,
            Some(InspectorDrag::Scrollbar { grab_offset: 1, .. })
        ));
        assert_eq!(state.inspector.preview_scroll, 0);
        mouse(
            &mut state,
            MouseEventKind::Drag(MouseButton::Left),
            region.vertical.x,
            thumb.top + 1,
        );
        assert_eq!(state.inspector.preview_scroll, 0);
        mouse(
            &mut state,
            MouseEventKind::Drag(MouseButton::Left),
            region.vertical.x,
            thumb.top + 2,
        );
        let expected = region.metrics(false).max_offset_from_bottom
            - crate::ui::scrollbar_offset_from_drag_row(
                region.metrics(false),
                region.track(false),
                thumb.top + 2,
                1,
            );
        assert_eq!(state.inspector.preview_scroll, expected);
    }

    #[test]
    fn inspector_display_cell_crop_preserves_graphemes_and_reference_column_semantics() {
        assert_eq!(crop_cells("a界b", 1, 2), "界");
        assert_eq!(crop_cells("a界b", 2, 2), " b");
        assert_eq!(crop_cells("a界b", 0, 2), "a ");
        assert_eq!(crop_cells("e\u{301}xy", 0, 1), "e\u{301}");
        assert_eq!(crop_cells("e\u{301}xy", 1, 1), "x");
        assert_eq!(crop_cells("👩‍💻z", 0, 2), "👩‍💻");
        assert_eq!(crop_cells("👩‍💻z", 1, 2), " z");
        assert_eq!(crop_cells("🙂z", 1, 2), " z");
        assert_eq!(crop_cells("界", 0, 0), "");
        assert_eq!(crop_cells("界", 9, 2), "");
        // Existing API column 2 means skip one scalar, NOT one display cell.
        assert_eq!(reference_line("界abc", Some(2)), "abc");
        assert_eq!(crop_cells(reference_line("界abc", Some(2)), 1, 2), "bc");
    }

    #[test]
    fn inspector_scroll_limits_follow_resize_content_source_and_fitting_edges() {
        for (rows, columns, width, height) in [
            (0, 0, 20, 10),
            (10, 20, 20, 10),
            (11, 20, 20, 10),
            (10, 21, 20, 10),
            (100, 100, 1, 1),
        ] {
            let region = InspectorRegion::new(
                Rect::new(10, 5, width, height),
                rows,
                columns,
                usize::MAX,
                usize::MAX,
            );
            assert_eq!(
                region.top,
                rows.saturating_sub(usize::from(region.content.height))
            );
            assert_eq!(
                region.left,
                columns.saturating_sub(usize::from(region.content.width))
            );
            if rows <= usize::from(height) && columns <= usize::from(width) {
                assert!(region.vertical.is_empty() && region.horizontal.is_empty());
                assert_eq!((region.top, region.left), (0, 0));
            }
        }
        let mut state = state();
        overflowing(&mut state);
        state.inspector.preview_scroll = usize::MAX;
        state.inspector.preview_horizontal_scroll = usize::MAX;
        state.inspector.list_scroll = usize::MAX;
        state.inspector.list_horizontal_scroll = usize::MAX;
        state.last_composed_size = Some((121, 30));
        state.clamp_inspector_scroll();
        let (list, preview) = state.inspector_regions(state.layout(121, 30).inspector);
        assert_eq!(
            state.inspector.preview_scroll,
            preview.rows - usize::from(preview.content.height)
        );
        assert_eq!(
            state.inspector.list_scroll,
            list.rows - usize::from(list.content.height)
        );
        state.last_composed_size = Some((240, 60));
        state.inspector.preview = "short".into();
        state.inspector.directories.clear();
        state.inspector.content = None;
        state.clamp_inspector_scroll();
        assert_eq!(
            (
                state.inspector.preview_scroll,
                state.inspector.preview_horizontal_scroll,
                state.inspector.list_scroll,
                state.inspector.list_horizontal_scroll
            ),
            (0, 0, 0, 0)
        );
        let (list, preview) = state.inspector_regions(state.layout(240, 60).inspector);
        assert!(
            list.vertical.is_empty()
                && list.horizontal.is_empty()
                && preview.vertical.is_empty()
                && preview.horizontal.is_empty()
        );
        state.inspector.preview_scroll = 10;
        state.reset_inspector_data(None);
        assert_eq!(state.inspector.preview_scroll, 0);
    }

    #[test]
    fn inspector_divider_drag_grows_beyond_default_and_retains_responsive_preference() {
        let mut state = state();
        open(&mut state);
        let area = state.layout(180, 40).inspector;
        let mut buffer = Buffer::empty(Rect::new(0, 0, 180, 40));
        state.render_inspector(&mut buffer, area);
        assert_eq!(buffer[(area.x, 10)].symbol(), "│");
        mouse(
            &mut state,
            MouseEventKind::Down(MouseButton::Left),
            area.x,
            10,
        );
        let grow = mouse(
            &mut state,
            MouseEventKind::Drag(MouseButton::Left),
            area.right() - 70,
            10,
        );
        assert!(grow.resize && grow.repaint && grow.requests.is_empty());
        assert!(state.pane_surface.is_none());
        assert_eq!(state.layout(180, 40).inspector.width, 70);
        assert!(mouse(
            &mut state,
            MouseEventKind::Up(MouseButton::Left),
            area.right() - 70,
            10
        )
        .requests
        .is_empty());
        state.reset_inspector_data(state.inspector_source());
        assert_eq!(state.inspector.preferred_width, Some(70));
        assert_eq!(state.layout(121, 30).inspector.width, 55);
        assert_eq!(state.layout(121, 30).pane_surface.width, 40);
        assert_eq!(state.layout(94, 30).inspector.width, 28);
        assert!(state.layout(93, 30).inspector.is_empty());
        assert!(state.layout(44, 30).inspector.is_empty());
        assert_eq!(state.layout(180, 40).inspector.width, 70);
        // Extreme pointer positions clamp without underflow or overlapping PTYs.
        let area = state.layout(180, 40).inspector;
        mouse(
            &mut state,
            MouseEventKind::Down(MouseButton::Left),
            area.x,
            10,
        );
        mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 0, 10);
        assert_eq!(state.layout(180, 40).pane_surface.width, 40);
        mouse(
            &mut state,
            MouseEventKind::Drag(MouseButton::Left),
            u16::MAX,
            10,
        );
        assert_eq!(state.layout(180, 40).inspector.width, MIN_WIDTH);
        assert!(
            mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 30, 10)
                .requests
                .is_empty()
        );
    }

    #[test]
    fn inspector_cancelled_drag_retains_release_sink_on_close_source_loss_and_mobile() {
        for cancel in 0..3 {
            let mut state = state();
            overflowing(&mut state);
            let area = state.layout(180, 40).inspector;
            let (_, region) = state.inspector_regions(area);
            mouse(
                &mut state,
                MouseEventKind::Down(MouseButton::Left),
                region.vertical.x,
                region.vertical.y,
            );
            match cancel {
                0 => {
                    open(&mut state);
                }
                1 => {
                    state.reset_inspector_data(None);
                }
                _ => {
                    state.last_composed_size = Some((44, 30));
                    state.clamp_inspector_scroll();
                }
            }
            assert!(state.inspector.drag.is_none());
            assert!(state.inspector.pointer_capture);
            let moved = mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 30, 10);
            let released = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 30, 10);
            assert!(moved.requests.is_empty() && released.requests.is_empty());
            assert!(state.inspector.drag.is_none() && !state.inspector.pointer_capture);
        }
    }

    #[test]
    fn inspector_resize_coalesces_raster_reads_without_discarding_queued_list_or_diff() {
        let mut state = state();
        open(&mut state);
        state.pending_requests.clear();
        state.inspector.queue.clear();
        state.inspector.raster = Some(CachedRaster {
            width: 1,
            height: 1,
            rgb: vec![0, 0, 0],
        });
        state.inspector.preview_file = Some(("/repo".into(), "synthetic.png".into()));
        state.inspector.raster_bounds = Some(state.preview_bounds());
        let area = state.layout(180, 40).inspector;
        mouse(
            &mut state,
            MouseEventKind::Down(MouseButton::Left),
            area.x,
            10,
        );
        for width in [50, 60, 70, 80] {
            let changed = mouse(
                &mut state,
                MouseEventKind::Drag(MouseButton::Left),
                area.right() - width,
                10,
            );
            assert!(changed.actions.is_empty());
            let mut tick = ClientShellInput::default();
            state.tick_inspector(&mut tick);
            assert!(tick.actions.is_empty() && state.inspector.queue.is_empty());
        }
        let list = InspectorRead::List("keep-list".into());
        let diff = InspectorRead::Diff {
            path: "keep-diff".into(),
            original_path: None,
            staged: false,
        };
        state.inspector.queue.extend([list.clone(), diff.clone()]);
        let release = mouse(
            &mut state,
            MouseEventKind::Up(MouseButton::Left),
            area.right() - 80,
            10,
        );
        let [ClientShellAction::Endpoint {
            request, boot_id, ..
        }] = &release.actions[..]
        else {
            panic!("queued list first");
        };
        assert!(matches!(&request.method, Method::FileList(params) if params.path == "keep-list"));
        assert_eq!(state.inspector.queue.front(), Some(&diff));
        let (_, actions) = state.handle_endpoint_result(boot_id, &request.id, Ok(list_result()));
        let [ClientShellAction::Endpoint {
            request, boot_id, ..
        }] = &actions[..]
        else {
            panic!("queued diff next");
        };
        assert!(matches!(&request.method, Method::GitDiff(params) if params.path == "keep-diff"));
        let (_, actions) = state.handle_endpoint_result(
            boot_id,
            &request.id,
            Ok(ResponseResult::GitDiff {
                text: "diff".into(),
                truncated: false,
            }),
        );
        let [ClientShellAction::Endpoint { request, .. }] = &actions[..] else {
            panic!("one final raster refresh");
        };
        assert!(
            matches!(&request.method, Method::FilePreview(params) if params.width == state.preview_bounds().0)
        );
        assert_eq!(state.pending_requests.len(), 1);
        assert!(state.inspector.queue.is_empty());
        let mut tick = ClientShellInput::default();
        state.tick_inspector(&mut tick);
        assert!(tick.actions.is_empty());
    }

    #[test]
    fn inspector_history_and_diff_preview_share_scroll_geometry() {
        let mut state = state();
        overflowing(&mut state);
        state.inspector.view = InspectorView::Log;
        state.inspector.log =
            "history with long overflowing lines ".repeat(5) + &"\nentry".repeat(100);
        state.inspector.content = None;
        state.clamp_inspector_scroll();
        let area = state.layout(180, 40).inspector;
        let (_, region) = state.inspector_regions(area);
        assert!(!region.vertical.is_empty() && !region.horizontal.is_empty());
        let down = mouse(
            &mut state,
            MouseEventKind::Down(MouseButton::Left),
            region.vertical.x,
            region.vertical.bottom() - 1,
        );
        assert!(down.requests.is_empty());
        assert!(state.inspector.preview_scroll > 0);
        mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 30, 10);
        state.inspector.view = InspectorView::Git;
        state.pending_requests.clear();
        let target = target(&state);
        state.complete_inspector(
            target,
            InspectorRead::Diff {
                path: "a.rs".into(),
                original_path: None,
                staged: false,
            },
            Ok(ResponseResult::GitDiff {
                text: "short diff".into(),
                truncated: false,
            }),
        );
        assert_eq!(state.inspector.preview_scroll, 0);
    }

    #[test]
    fn inspector_geometry_is_collapsed_by_default_and_preserves_terminal_mobile_and_tab_edges() {
        let mut state = state();
        assert!(state.layout(180, 40).inspector.is_empty());
        let before = state.surface_size(180, 40);
        let opened = open(&mut state);
        assert!(opened.resize && opened.repaint);
        let layout = state.layout(180, 40);
        assert_eq!(
            state.surface_size(180, 40).cols,
            before.cols - DEFAULT_WIDTH
        );
        assert!(layout.pane_surface.right() <= layout.inspector.x);
        assert!(layout.tab_bar.right() <= layout.inspector.x);
        for (cols, rows) in [(0, 0), (1, 1), (44, 20), (93, 30), (180, 4)] {
            assert!(state.layout(cols, rows).inspector.is_empty());
        }
        for position in [TabBarPositionConfig::Top, TabBarPositionConfig::Bottom] {
            state.config.tab_bar_position = position;
            let layout = state.layout(180, 40);
            assert_eq!(layout.inspector.height, 40);
            assert!(layout
                .pane_surface
                .intersection(layout.inspector)
                .is_empty());
        }
        let closed = open(&mut state);
        assert!(closed.resize && closed.repaint);
        assert_eq!(state.surface_size(180, 40), before);
    }

    #[test]
    fn dormant_inspector_opens_and_renders_at_the_reported_121_column_size() {
        let mut state = state();
        state.last_composed_size = Some((121, 30));
        let mut outcome = ClientShellInput::default();
        state.toggle_inspector(&mut outcome);
        let layout = state.layout(121, 30);
        assert!(state.inspector.open && outcome.resize && outcome.repaint);
        assert!(state.overlay.is_none());
        assert_eq!(layout.inspector.width, 42);
        assert_eq!(layout.pane_surface.width, 53);
        assert!(state.visible_endpoint_notice.is_none());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 121, 30));
        state.render_inspector(&mut buffer, layout.inspector);
        let header = (layout.inspector.x..layout.inspector.right())
            .map(|x| buffer[(x, 1)].symbol())
            .collect::<String>();
        assert!(header.contains("[Files]"));
        assert!(
            !outcome.actions.is_empty(),
            "opening must schedule the file list"
        );
    }

    #[test]
    fn inspector_width_adapts_without_overlapping_terminal_or_tabs() {
        for position in [TabBarPositionConfig::Top, TabBarPositionConfig::Bottom] {
            for (cols, expected) in [(121, 42), (110, 42), (100, 34), (94, 28)] {
                let mut state = state();
                state.config.tab_bar_position = position;
                state.last_composed_size = Some((cols, 30));
                open(&mut state);
                let layout = state.layout(cols, 30);
                assert_eq!(layout.inspector.width, expected, "cols={cols}");
                assert!(layout.pane_surface.width >= MIN_TERMINAL_WIDTH);
                assert!(layout
                    .pane_surface
                    .intersection(layout.inspector)
                    .is_empty());
                assert!(layout.tab_bar.intersection(layout.inspector).is_empty());
            }
        }
    }

    #[test]
    fn inspector_hidden_geometry_has_feedback_without_requests_and_recovers_on_resize() {
        for (cols, rows) in [(93, 30), (44, 30), (121, 4)] {
            let mut state = state();
            state.last_composed_size = Some((cols, rows));
            let outcome = open(&mut state);
            assert!(state.layout(cols, rows).inspector.is_empty());
            assert!(outcome.actions.is_empty());
            let notice = state.visible_endpoint_notice.as_ref().unwrap();
            assert_eq!(notice.key.kind, ClientEndpointNoticeKind::Rejected);
            assert_eq!(notice.key.code, "inspector_geometry");
            assert!(notice.body.contains("Widen"));
            state.sync_inspector_source();
            assert!(state.inspector.source.is_some());
            state.last_composed_size = Some((121, 30));
            let mut resumed = ClientShellInput::default();
            state.tick_inspector(&mut resumed);
            assert!(state.visible_endpoint_notice.is_none());
            assert!(resumed.repaint, "removing a visible notice must repaint");
            assert!(!resumed.actions.is_empty());
        }
    }

    #[test]
    fn inspector_geometry_feedback_can_be_shown_again_after_closing() {
        let mut state = state();
        state.last_composed_size = Some((93, 30));
        open(&mut state);
        assert!(state.visible_endpoint_notice.is_some());
        open(&mut state);
        assert!(state.visible_endpoint_notice.is_none());
        open(&mut state);
        assert!(state.visible_endpoint_notice.is_some());
    }

    #[test]
    fn inspector_minimum_width_tabs_and_close_button_are_clickable() {
        let mut state = state();
        state.last_composed_size = Some((94, 30));
        open(&mut state);
        state.compose(94, 30).unwrap();
        let area = state.layout(94, 30).inspector;
        assert_eq!(area.width, MIN_WIDTH);
        mouse(
            &mut state,
            MouseEventKind::Down(MouseButton::Left),
            area.x + 10,
            area.y + 1,
        );
        assert!(state.inspector.view == InspectorView::Git);
        mouse(
            &mut state,
            MouseEventKind::Up(MouseButton::Left),
            area.x + 10,
            area.y + 1,
        );
        mouse(
            &mut state,
            MouseEventKind::Down(MouseButton::Left),
            area.right() - 3,
            area.y,
        );
        assert!(!state.inspector.open);
    }

    #[test]
    fn inspector_stale_completions_are_rejected_for_pane_cwd_boot_connection_endpoint_and_selection(
    ) {
        for change in 0..6 {
            let mut state = state();
            open(&mut state);
            let old = target(&state);
            match change {
                0 => {
                    state.snapshot.as_mut().unwrap().focused_pane_id = Some("other-pane".into());
                }
                1 => {
                    state.snapshot.as_mut().unwrap().panes[0].foreground_cwd =
                        Some("/different".into());
                }
                2 => {
                    state.snapshot.as_mut().unwrap().boot_id = "reboot".into();
                }
                3 => {
                    state.active_snapshot_generation = Some(42);
                }
                4 => {
                    state.active_endpoint_id = ClientEndpointId::Ssh(
                        crate::client::endpoint::ProfileId::parse(
                            "0123456789abcdef0123456789abcdef",
                        )
                        .unwrap(),
                    );
                }
                _ => {
                    state.inspector.generation += 1;
                }
            }
            state.complete_inspector(old, InspectorRead::List(String::new()), Ok(list_result()));
            assert!(
                state.inspector.directories.is_empty(),
                "stale case {change}"
            );
        }
        let mut state = state();
        open(&mut state);
        let old = target(&state);
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.panes[0].foreground_cwd = Some("/other".into());
        state.set_snapshot(Box::new(snapshot));
        state.set_snapshot(Box::new(crate::client::shell::tests::snapshot()));
        assert!(
            !state.inspector_target_current(&old),
            "A -> B -> A must invalidate the original generation too"
        );
    }

    #[test]
    fn inspector_remote_requests_never_fall_back_to_local_and_use_focused_foreground_cwd() {
        let mut state = state();
        let profile = SavedSshEndpoint {
            id: crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
                .unwrap(),
            label: "Remote".into(),
            target: "remote".into(),
            session: "test".into(),
            enabled: true,
        };
        let remote = ClientEndpointId::Ssh(profile.id.clone());
        state.set_endpoint_catalog(&[profile]);
        state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
        let mut projection = crate::client::shell::tests::snapshot();
        projection.panes[0].foreground_cwd = Some("/remote-only/missing-on-client".into());
        state.set_endpoint_snapshot(&remote, Box::new(projection));
        assert!(state.activate_endpoint_projection(&remote));
        state.last_composed_size = Some((180, 40));
        state.set_endpoint_methods(Some(vec!["file.list".into()]));
        let outcome = open(&mut state);
        assert!(outcome.requests.is_empty());
        let [ClientShellAction::Endpoint {
            endpoint_id,
            request,
            ..
        }] = &outcome.actions[..]
        else {
            panic!("one remote request");
        };
        assert_eq!(endpoint_id, &remote);
        assert!(
            matches!(&request.method, Method::FileList(params) if params.root == "/remote-only/missing-on-client")
        );
    }

    #[test]
    fn inspector_unsupported_and_closed_do_not_schedule_reads_or_disable_terminal_input() {
        let mut state = state();
        state.set_endpoint_methods(Some(Vec::new()));
        assert!(open(&mut state).actions.is_empty());
        assert!(state
            .inspector
            .message
            .contains("does not support file.list"));
        let typing = state.handle_input_bytes(b"echo test\r");
        assert!(!typing.requests.is_empty());
        open(&mut state);
        state.inspector.queue.push_back(InspectorRead::Status);
        let mut outcome = ClientShellInput::default();
        state.tick_inspector(&mut outcome);
        assert!(outcome.actions.is_empty());
        state.inspector.open = true;
        state.last_composed_size = Some((44, 20));
        state.tick_inspector(&mut outcome);
        assert!(
            outcome.actions.is_empty(),
            "mobile hidden panel must not refresh"
        );
    }

    #[test]
    fn inspector_click_wheel_drag_release_do_not_leak_to_pty_and_typing_still_does() {
        let mut state = state();
        open(&mut state);
        let old = target(&state);
        state.pending_requests.clear();
        state.complete_inspector(old, InspectorRead::List(String::new()), Ok(list_result()));
        let area = state.layout(180, 40).inspector;
        let list = ClientShellState::inspector_list_area(area);
        let click = mouse(
            &mut state,
            MouseEventKind::Down(MouseButton::Left),
            list.x,
            list.y,
        );
        assert!(click.requests.is_empty());
        assert!(click.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::FilePreview(_)))));
        for kind in [
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollUp,
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            assert!(mouse(&mut state, kind, area.x + 1, area.bottom() - 1)
                .requests
                .is_empty());
        }
        assert!(mouse(
            &mut state,
            MouseEventKind::Down(MouseButton::Left),
            list.x,
            list.y
        )
        .requests
        .is_empty());
        assert!(
            mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 30, 20)
                .requests
                .is_empty()
        );
        assert!(
            mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 30, 20)
                .requests
                .is_empty()
        );
        let typing = state.handle_input_bytes(b"abc\r");
        assert!(typing
            .requests
            .iter()
            .any(|request| matches!(request, ClientMessage::ClientShellPaneInput { .. })));
        assert!(typing.actions.is_empty());
    }

    #[test]
    fn inspector_preserves_terminal_origin_gesture_release_across_panel() {
        let mut state = state();
        open(&mut state);
        let hit = PaneHit {
            rect: Rect::new(26, 1, 112, 39),
            inner_rect: Rect::new(26, 1, 112, 39),
            scrollbar_rect: None,
            scroll: None,
            pane_id: "pane_1".into(),
            popup: false,
            mouse_reporting: true,
            sgr_pixel_mouse: false,
            pixel_width: 0,
            pixel_height: 0,
        };
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 30,
            row: 10,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        state.pane_mouse_gesture = Some(ClientPaneMouseGesture {
            hit,
            button: MouseButton::Left,
            stripped_modifiers: crossterm::event::KeyModifiers::empty(),
            last_event: event,
            last_position: ClientMousePosition::Cell { column: 4, row: 9 },
        });
        let area = state.layout(180, 40).inspector;
        let release = mouse(
            &mut state,
            MouseEventKind::Up(MouseButton::Left),
            area.x + 1,
            10,
        );
        assert!(state.pane_mouse_gesture.is_none());
        assert!(release
            .requests
            .iter()
            .any(|request| matches!(request, ClientMessage::ClientShellPaneInput { .. })));
    }

    #[test]
    fn inspector_pending_lane_coalesces_and_close_discards_result_without_refresh() {
        let mut state = state();
        let opening = open(&mut state);
        let [ClientShellAction::Endpoint {
            boot_id, request, ..
        }] = &opening.actions[..]
        else {
            panic!("read request");
        };
        let mut tick = ClientShellInput::default();
        for _ in 0..5 {
            state.tick_inspector(&mut tick);
        }
        assert!(tick.actions.is_empty());
        assert_eq!(state.pending_requests.len(), 1);
        let closing = open(&mut state);
        assert!(closing.resize && closing.actions.is_empty());
        let (_, followup) = state.handle_endpoint_result(boot_id, &request.id, Ok(list_result()));
        assert!(followup.is_empty());
        assert!(state.inspector.directories.is_empty());
        state.tick_inspector(&mut tick);
        assert!(tick.actions.is_empty());
    }

    #[test]
    fn inspector_popup_and_overlay_keep_existing_mouse_priority_and_close_hit_matches_button() {
        let mut state = state();
        open(&mut state);
        let area = state.layout(180, 40).inspector;
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.right() - 3,
            row: area.y,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        let mut outcome = ClientShellInput::default();
        state.popup_pending = true;
        assert!(!state.handle_inspector_mouse(event, &mut outcome));
        state.popup_pending = false;
        state.overlay = Some(ClientShellOverlay::GlobalMenu(ClientGlobalMenuOverlay {
            highlighted: 0,
        }));
        assert!(!state.handle_inspector_mouse(event, &mut outcome));
        state.overlay = None;
        assert!(state.handle_inspector_mouse(event, &mut outcome));
        assert!(!state.inspector.open && outcome.resize);
        assert!(
            mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 30, 20)
                .requests
                .is_empty()
        );
    }

    #[test]
    fn inspector_escapes_controls_but_keeps_raw_request_paths_and_preview_lines() {
        assert_eq!(clean("a\n\u{1b}[31m"), "a\\n\\u{1b}[31m");
        assert!(!clean("\u{202e}evil").contains('\u{202e}'));
        assert_eq!(clean_lines("one\ntwo\u{1b}"), "one\ntwo\\u{1b}");
        let mut state = state();
        open(&mut state);
        state.inspector.directories.insert(
            String::new(),
            vec![FileEntry {
                path: "a\n\u{1b}".into(),
                directory: false,
                symlink: false,
            }],
        );
        state.inspector.content = None;
        state.clamp_inspector_scroll();
        let rows = state.inspector_rows();
        assert!(!rows[0].0.contains('\n'));
        assert!(matches!(&rows[0].1, InspectorRead::File { path, .. } if path == "a\n\u{1b}"));
    }
    #[test]
    fn file_reference_open_waits_for_source_focus_and_cancels_stale_intent() {
        for stale in [false, true] {
            let mut state = state();
            state.set_endpoint_methods(Some(
                [
                    "pane.file_link.activate",
                    "file.preview",
                    "file.list",
                    "pane.focus",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            ));
            let mut snapshot = crate::client::shell::tests::snapshot();
            let mut source = snapshot.panes[0].clone();
            source.pane_id = "pane_2".into();
            source.cwd = Some("/source".into());
            source.foreground_cwd = Some("/source".into());
            snapshot.panes.push(source);
            state.set_snapshot(Box::new(snapshot.clone()));
            let params = crate::api::schema::PaneLinkActivateParams {
                pane_id: "pane_2".into(),
                viewport_row: 0,
                col: 0,
                content_revision: None,
                offset_from_bottom: None,
            };
            let attempt = state.file_link_attempt(params).unwrap();
            let reference = crate::api::schema::FileReference {
                path: "/source/src/a.rs".into(),
                line: Some(3),
                column: Some(2),
            };
            let (_, actions) = state.complete_file_link(
                attempt,
                Rect::default(),
                Vec::new(),
                Ok(ResponseResult::FileReference {
                    reference: Some(reference),
                }),
            );
            assert!(actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::PaneFocus(_)))));
            let mut outcome = ClientShellInput::default();
            state.tick_inspector(&mut outcome);
            assert!(outcome.actions.is_empty() && !state.inspector.open);
            if stale {
                snapshot.panes[1].foreground_cwd = Some("/changed".into());
            }
            snapshot.focused_pane_id = Some("pane_2".into());
            state.set_snapshot(Box::new(snapshot));
            state.tick_inspector(&mut outcome);
            if stale {
                assert!(!state.inspector.open && outcome.actions.is_empty());
            } else {
                assert!(state.inspector.open && outcome.resize);
                assert_eq!(state.inspector.preview_line, Some(3));
                assert_eq!(state.inspector.preview_column, Some(2));
                assert!(outcome.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::FilePreview(params) if params.root == "/source" && params.path == "src/a.rs"))));
            }
        }
        assert!(relative_reference("/source", "/outside/private").is_none());
        assert!(relative_reference("/source", "../outside").is_none());
    }

    #[test]
    fn raster_response_validation_bounds_pixels_and_nested_envelope() {
        use base64::Engine;
        let raster = PreviewRaster {
            width: 160,
            height: 160,
            original_width: 8192,
            original_height: 1,
            rgb_base64: base64::engine::general_purpose::STANDARD.encode(vec![255; 160 * 160 * 3]),
        };
        let response = crate::api::schema::SuccessResponse {
            id: "test".into(),
            result: ResponseResult::FilePreview {
                content: FilePreviewContent::Image {
                    image: raster.clone(),
                },
            },
        };
        let json = serde_json::to_string(&response).unwrap();
        let nested = serde_json::to_string(&serde_json::json!({"data":json})).unwrap();
        assert!(nested.len() < 1024 * 1024);
        assert!(validate_raster(raster.clone()).is_ok());
        assert!(validate_raster(PreviewRaster {
            width: 161,
            ..raster.clone()
        })
        .is_err());
        assert!(validate_raster(PreviewRaster {
            rgb_base64: "bad".into(),
            ..raster
        })
        .is_err());
    }
    #[test]
    fn pdf_page_buttons_share_fixed_paint_and_hit_positions_for_two_page_documents() {
        let mut state = state();
        open(&mut state);
        state.pending_requests.clear();
        state.inspector.preview_file = Some(("/repo".into(), "fixture.pdf".into()));
        state.inspector.page = 1;
        state.inspector.pages = 2;
        let area = state.layout(180, 40).inspector;
        let row = ClientShellState::inspector_list_area(area).bottom();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 180, 40));
        state.render_inspector(&mut buffer, area);
        assert_eq!(buffer.cell((area.right() - 3, row)).unwrap().symbol(), "+");
        assert_eq!(buffer.cell((area.right() - 17, row)).unwrap().symbol(), "-");
        let next = mouse(
            &mut state,
            MouseEventKind::Down(MouseButton::Left),
            area.right() - 3,
            row,
        );
        assert_eq!(state.inspector.page, 2);
        assert!(next.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::FilePreview(params) if params.page == 2))));
        mouse(
            &mut state,
            MouseEventKind::Up(MouseButton::Left),
            area.right() - 3,
            row,
        );
        state.pending_requests.clear();
        let previous = mouse(
            &mut state,
            MouseEventKind::Down(MouseButton::Left),
            area.right() - 17,
            row,
        );
        assert_eq!(state.inspector.page, 1);
        assert!(previous.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::FilePreview(params) if params.page == 1))));
    }
    #[test]
    fn cancelled_inspector_file_does_not_dispatch_queued_listing_or_register_phantom_request() {
        let mut state = state();
        open(&mut state);
        state.pending_requests.clear();
        state.inspector.queue.clear();
        state.inspector.queue.push_back(InspectorRead::File {
            root: "/repo".into(),
            path: "a.rs".into(),
        });
        state
            .inspector
            .queue
            .push_back(InspectorRead::List(String::new()));
        let mut outcome = ClientShellInput::default();
        state.tick_inspector(&mut outcome);
        let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
            panic!("file read");
        };
        let serial = state.next_request_id;
        state.cancel_endpoint_request(&request.id);
        assert!(state.pending_requests.is_empty());
        assert_eq!(state.next_request_id, serial);
        assert_eq!(
            state.inspector.queue.iter().cloned().collect::<Vec<_>>(),
            vec![InspectorRead::List(String::new())]
        );
    }

    fn ctrl_file_link_state() -> ClientShellState {
        let mut state = state();
        state.config.mouse_capture = true;
        state.set_endpoint_methods(Some(
            [
                "pane.link.activate",
                "pane.file_link.activate",
                "file.preview",
                "file.list",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        ));
        state.hits.panes.push(PaneHit {
            rect: Rect::new(26, 1, 112, 39),
            inner_rect: Rect::new(26, 1, 112, 39),
            scrollbar_rect: None,
            scroll: None,
            pane_id: "pane_1".into(),
            popup: false,
            mouse_reporting: true,
            sgr_pixel_mouse: false,
            pixel_width: 0,
            pixel_height: 0,
        });
        state
    }

    fn ctrl_file_link_down(state: &mut ClientShellState) -> String {
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 27,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::CONTROL,
        };
        let outcome = state.handle_raw_events(vec![crate::raw_input::RawInputEvent::Mouse(down)]);
        assert!(
            outcome.requests.is_empty(),
            "CtrlDown must not reach the pane app"
        );
        let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
            panic!("expected legacy link activation only");
        };
        assert!(matches!(&request.method, Method::PaneLinkActivate(params)
            if params.pane_id == "pane_1" && params.viewport_row == 1 && params.col == 1));
        request.id.clone()
    }

    #[test]
    fn withdrawn_presentation_stale_file_link_completions_never_replay_or_open() {
        for file_stage in [false, true] {
            let mut state = ctrl_file_link_state();
            let legacy_id = ctrl_file_link_down(&mut state);
            let id = if file_stage {
                let (_, actions) = state.handle_endpoint_result(
                    "boot-1",
                    &legacy_id,
                    Ok(ResponseResult::PaneLinkActivated {
                        url: None,
                        handled: false,
                    }),
                );
                let [ClientShellAction::Endpoint { request, .. }] = &actions[..] else {
                    panic!("fixture must reach the dormant file-link lane");
                };
                request.id.clone()
            } else {
                legacy_id
            };
            state.retired_presentation_fixture = false;
            let serial = state.next_request_id;
            let result = if file_stage {
                ResponseResult::FileReference {
                    reference: Some(crate::api::schema::FileReference {
                        path: "README.md".into(),
                        line: Some(1),
                        column: None,
                    }),
                }
            } else {
                ResponseResult::PaneLinkActivated {
                    url: None,
                    handled: false,
                }
            };
            let (_, actions) = state.handle_endpoint_result("boot-1", &id, Ok(result));
            assert!(
                actions.is_empty(),
                "withdrawn file routes must not replay coordinates"
            );
            assert!(state.pending_requests.is_empty() && state.pending_file_open.is_none());
            let mut outcome = ClientShellInput::default();
            state.tick_inspector(&mut outcome);
            assert!(outcome.actions.is_empty() && !state.inspector.open);
            assert_eq!(state.next_request_id, serial);
        }
    }

    #[test]
    fn withdrawn_presentation_stale_inspector_capture_and_raster_completion_are_inert() {
        let mut state = state();
        open(&mut state);
        state.pending_requests.clear();
        state.inspector.queue.clear();
        state.inspector.queue.push_back(InspectorRead::File {
            root: "/repo".into(),
            path: "fixture.pdf".into(),
        });
        let mut outcome = ClientShellInput::default();
        state.tick_inspector(&mut outcome);
        let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
            panic!("fixture must reach the dormant preview lane");
        };
        let id = request.id.clone();
        state.inspector.pointer_capture = true;
        state.inspector.drag = Some(InspectorDrag::Width);
        state.retired_presentation_fixture = false;
        let serial = state.next_request_id;
        let builds = state.inspector.content_builds;
        let (_, actions) = state.handle_endpoint_result(
            "boot-1",
            &id,
            Ok(ResponseResult::FilePreview {
                content: FilePreviewContent::Pdf {
                    image: PreviewRaster {
                        width: 1,
                        height: 1,
                        original_width: 1,
                        original_height: 1,
                        rgb_base64: "////".into(),
                    },
                    page: 1,
                    pages: 2,
                },
            }),
        );
        assert!(actions.is_empty() && state.inspector.raster.is_none());
        assert_eq!(state.inspector.content_builds, builds);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::ScrollDown,
        ] {
            let mut outcome = ClientShellInput::default();
            assert!(!state.handle_inspector_mouse(
                MouseEvent {
                    kind,
                    column: 170,
                    row: 4,
                    modifiers: crossterm::event::KeyModifiers::empty(),
                },
                &mut outcome
            ));
            state.tick_inspector(&mut outcome);
            assert!(outcome.actions.is_empty() && outcome.requests.is_empty());
        }
        assert_eq!(state.next_request_id, serial);
        assert!(state.pending_requests.is_empty());
        assert!(state.layout(180, 40).inspector.is_empty());
    }

    #[test]
    fn ctrl_click_basenames_with_mouse_reporting_open_inspector_through_legacy_file_chain() {
        for (path, line) in [("README.md", None), ("AGENTS.md", Some(12))] {
            let mut state = ctrl_file_link_state();
            let legacy_id = ctrl_file_link_down(&mut state);
            let (_, actions) = state.handle_endpoint_result(
                "boot-1",
                &legacy_id,
                Ok(ResponseResult::PaneLinkActivated {
                    url: None,
                    handled: false,
                }),
            );
            let [ClientShellAction::Endpoint { request, .. }] = &actions[..] else {
                panic!("expected file link activation, never PTY replay");
            };
            assert!(
                matches!(&request.method, Method::PaneFileLinkActivate(params)
                if params.pane_id == "pane_1" && params.viewport_row == 1 && params.col == 1)
            );
            let file_id = request.id.clone();
            let (_, actions) = state.handle_endpoint_result(
                "boot-1",
                &file_id,
                Ok(ResponseResult::FileReference {
                    reference: Some(crate::api::schema::FileReference {
                        path: path.into(),
                        line,
                        column: None,
                    }),
                }),
            );
            assert!(actions.is_empty());
            let mut outcome = ClientShellInput::default();
            state.tick_inspector(&mut outcome);
            assert!(state.inspector.open && outcome.resize);
            assert_eq!(state.inspector.preview_line, line);
            assert!(outcome.actions.iter().any(|action| matches!(action,
                ClientShellAction::Endpoint { request, .. }
                    if matches!(&request.method, Method::FilePreview(params)
                        if params.root == "/repo" && params.path == path))));
        }
    }

    #[test]
    fn ctrl_click_stale_legacy_link_notifies_without_retry_but_cancelled_or_changed_source_is_silent(
    ) {
        for code in ["stale_content", "stale_target", "endpoint_cancelled"] {
            for changed_source in [false, true] {
                let mut state = ctrl_file_link_state();
                let legacy_id = ctrl_file_link_down(&mut state);
                if changed_source {
                    let pane = &mut state.snapshot.as_mut().unwrap().panes[0];
                    pane.cwd = Some("/changed".into());
                    pane.foreground_cwd = Some("/changed".into());
                }
                let serial = state.next_request_id;
                let (repaint, actions) = state.handle_endpoint_result(
                    "boot-1",
                    &legacy_id,
                    Err(ClientShellEndpointError {
                        code: Some(code.into()),
                        message: "synthetic stale/cancelled".into(),
                    }),
                );
                assert!(actions.is_empty());
                assert!(state.pending_requests.is_empty());
                assert_eq!(state.next_request_id, serial, "no automatic retry");
                assert!(state.url_click_consumes_until_up);
                assert!(state.pending_file_open.is_none());
                if changed_source || code == "endpoint_cancelled" {
                    assert!(!repaint);
                    assert!(state.visible_endpoint_notice.is_none());
                } else {
                    assert!(repaint);
                    let notice = state.visible_endpoint_notice.as_ref().unwrap();
                    assert_eq!(notice.key.kind, ClientEndpointNoticeKind::Rejected);
                    assert_eq!(notice.key.code, "file_link_changed");
                }
            }
        }
    }

    #[test]
    fn stale_and_cancelled_file_link_results_never_replay_terminal_gestures() {
        for code in ["stale_content", "stale_target", "endpoint_cancelled"] {
            let mut state = state();
            state.set_endpoint_methods(Some(vec![
                "file.preview".into(),
                "pane.file_link.activate".into(),
            ]));
            let hit = PaneHit {
                rect: Rect::new(26, 1, 112, 39),
                inner_rect: Rect::new(26, 1, 112, 39),
                scrollbar_rect: None,
                scroll: None,
                pane_id: "pane_1".into(),
                popup: false,
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                pixel_width: 0,
                pixel_height: 0,
            };
            state.hits.panes.push(hit.clone());
            let params = crate::api::schema::PaneLinkActivateParams {
                pane_id: "pane_1".into(),
                viewport_row: 0,
                col: 0,
                content_revision: Some(2),
                offset_from_bottom: Some(0),
            };
            let attempt = state.file_link_attempt(params).unwrap();
            let down = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 27,
                row: 2,
                modifiers: crossterm::event::KeyModifiers::CONTROL,
            };
            let (repaint, actions) = state.complete_file_link(
                attempt,
                hit.inner_rect,
                vec![down],
                Err(ClientShellEndpointError {
                    code: Some(code.into()),
                    message: "synthetic changed content/viewport".into(),
                }),
            );
            assert!(actions.is_empty(), "{code}");
            assert!(state.url_click_consumes_until_up);
            assert!(state.pending_file_open.is_none());
            assert_eq!(repaint, code != "endpoint_cancelled");
            if code == "endpoint_cancelled" {
                assert!(state.visible_endpoint_notice.is_none());
            } else {
                let notice = state.visible_endpoint_notice.as_ref().unwrap();
                assert_eq!(notice.key.kind, ClientEndpointNoticeKind::Rejected);
                assert_eq!(notice.key.code, "file_link_changed");
            }
        }
    }

    #[test]
    fn explicit_close_or_view_selection_cancels_pending_source_focus_intent() {
        for close in [false, true] {
            let mut state = state();
            open(&mut state);
            state.set_endpoint_methods(Some(
                ["file.preview", "pane.file_link.activate", "git.status"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            ));
            let mut snapshot = crate::client::shell::tests::snapshot();
            let mut source = snapshot.panes[0].clone();
            source.pane_id = "pane_2".into();
            snapshot.panes.push(source);
            state.set_snapshot(Box::new(snapshot.clone()));
            let attempt = state
                .file_link_attempt(crate::api::schema::PaneLinkActivateParams {
                    pane_id: "pane_2".into(),
                    viewport_row: 0,
                    col: 0,
                    content_revision: None,
                    offset_from_bottom: None,
                })
                .unwrap();
            state.pending_file_open = Some(PendingFileOpen {
                attempt,
                reference: crate::api::schema::FileReference {
                    path: "old.rs".into(),
                    line: Some(1),
                    column: None,
                },
            });
            let area = state.layout(180, 40).inspector;
            mouse(
                &mut state,
                MouseEventKind::Down(MouseButton::Left),
                if close { area.right() - 3 } else { area.x + 10 },
                if close { area.y } else { area.y + 1 },
            );
            assert!(state.pending_file_open.is_none());
            snapshot.focused_pane_id = Some("pane_2".into());
            state.set_snapshot(Box::new(snapshot));
            let mut outcome = ClientShellInput::default();
            state.tick_inspector(&mut outcome);
            assert_eq!(state.inspector.open, !close);
            assert!(!outcome.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::FilePreview(_)))));
        }
    }

    #[test]
    fn inspector_close_works_without_pane_cwd_or_online_endpoint() {
        for missing in 0..3 {
            let mut state = state();
            open(&mut state);
            match missing {
                0 => state.snapshot.as_mut().unwrap().focused_pane_id = None,
                1 => {
                    let pane = &mut state.snapshot.as_mut().unwrap().panes[0];
                    pane.cwd = None;
                    pane.foreground_cwd = None;
                }
                _ => state.endpoints[0].status = ClientEndpointStatus::Reconnecting,
            }
            let area = state.layout(180, 40).inspector;
            let outcome = mouse(
                &mut state,
                MouseEventKind::Down(MouseButton::Left),
                area.right() - 3,
                area.y,
            );
            assert!(
                !state.inspector.open && outcome.resize,
                "missing source case {missing}"
            );
            assert!(outcome.actions.is_empty());
        }
    }

    #[test]
    fn hidden_same_source_read_is_requeued_once_only_after_visibility_returns() {
        for preview in [false, true] {
            let mut state = state();
            let mut outcome = open(&mut state);
            if preview {
                state.pending_requests.clear();
                state.inspector.queue.clear();
                outcome.actions.clear();
                state.inspector.queue.push_back(InspectorRead::File {
                    root: "/repo".into(),
                    path: "a.rs".into(),
                });
                state.tick_inspector(&mut outcome);
            }
            let [ClientShellAction::Endpoint {
                request, boot_id, ..
            }] = &outcome.actions[..]
            else {
                panic!("read");
            };
            state.last_composed_size = Some((44, 20));
            let result = if preview {
                ResponseResult::FilePreview {
                    content: FilePreviewContent::Text {
                        text: "test".into(),
                        truncated: false,
                    },
                }
            } else {
                list_result()
            };
            let (_, actions) = state.handle_endpoint_result(boot_id, &request.id, Ok(result));
            assert!(actions.is_empty() && state.pending_requests.is_empty());
            assert_eq!(state.inspector.queue.len(), 1);
            let mut hidden = ClientShellInput::default();
            state.tick_inspector(&mut hidden);
            assert!(hidden.actions.is_empty());
            state.last_composed_size = Some((180, 40));
            let mut shown = ClientShellInput::default();
            state.tick_inspector(&mut shown);
            assert_eq!(shown.actions.len(), 1);
            assert!(state.inspector.queue.is_empty());
            let mut repeated = ClientShellInput::default();
            state.tick_inspector(&mut repeated);
            assert!(repeated.actions.is_empty());
        }
    }
}
