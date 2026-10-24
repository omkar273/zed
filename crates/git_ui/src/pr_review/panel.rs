//! The Pull Requests panel: the repository's open pull requests grouped like
//! VS Code's GitHub Pull Requests view, each expanding to its changed files.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::Result;
use file_icons::FileIcons;
use gpui::{
    Action, Anchor, AnyElement, App, AsyncWindowContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, KeyContext, Pixels, PromptLevel, ScrollStrategy, Stateful,
    Subscription, UniformListScrollHandle, WeakEntity, Window, actions, uniform_list,
};
use project::{Project, git_store::GitStoreEvent};
use ui::{Checkbox, ContextMenu, DiffStat, PopoverMenu, ToggleState, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use super::OpenTarget;
use super::github::{
    self, ChangeType, CheckStatus, PullRequestFile, PullRequestFiles, PullRequestSummary, Section,
};

actions!(
    pr_review,
    [
        /// Toggles focus on the Pull Requests panel.
        TogglePullRequestsPanel,
    ]
);

const PANEL_KEY: &str = "PullRequestsPanel";
const ROW_GROUP: &str = "pull-requests-row";
const INDENT_STEP: f32 = 12.;
const CHEVRON_WIDTH: f32 = 16.;

fn row_height() -> Rems {
    rems(1.375)
}

fn panel_icon_button(id: impl Into<ElementId>, icon: IconName) -> IconButton {
    IconButton::new(id, icon)
        .icon_size(IconSize::Small)
        .size(ButtonSize::Compact)
}

enum LoadState<T> {
    Loading,
    Loaded(T),
    Failed(SharedString),
}

#[derive(Clone)]
enum Row {
    Section {
        section: Section,
        expanded: bool,
        count: Option<usize>,
    },
    Message {
        depth: usize,
        text: SharedString,
        is_error: bool,
    },
    PullRequest {
        section: Section,
        pull_request: PullRequestSummary,
        expanded: bool,
    },
    AllChanges {
        number: u32,
    },
    Directory {
        section: Section,
        number: u32,
        path: String,
        name: String,
        depth: usize,
        expanded: bool,
        viewed: ToggleState,
        file_paths: Vec<String>,
    },
    File {
        number: u32,
        file: PullRequestFile,
        name: String,
        depth: usize,
    },
}

impl Row {
    fn depth(&self) -> usize {
        match self {
            Row::Section { .. } => 0,
            Row::PullRequest { .. } => 1,
            Row::AllChanges { .. } => 2,
            Row::Message { depth, .. } | Row::Directory { depth, .. } | Row::File { depth, .. } => {
                *depth
            }
        }
    }

    /// `Some(expanded)` for rows that can be expanded and collapsed.
    fn expanded(&self) -> Option<bool> {
        match self {
            Row::Section { expanded, .. }
            | Row::PullRequest { expanded, .. }
            | Row::Directory { expanded, .. } => Some(*expanded),
            Row::Message { .. } | Row::AllChanges { .. } | Row::File { .. } => None,
        }
    }

    fn loading(depth: usize) -> Self {
        Row::Message {
            depth,
            text: "Loading…".into(),
            is_error: false,
        }
    }
}

/// A pull request's changed files arranged as folders, before compaction.
#[derive(Default)]
struct FileTree {
    directories: BTreeMap<String, FileTree>,
    files: Vec<usize>,
}

impl FileTree {
    fn new(files: &[PullRequestFile]) -> Self {
        let mut root = FileTree::default();
        for (index, file) in files.iter().enumerate() {
            let mut node = &mut root;
            if let Some((directory, _)) = file.path.rsplit_once('/') {
                for component in directory.split('/') {
                    node = node.directories.entry(component.to_string()).or_default();
                }
            }
            node.files.push(index);
        }
        root
    }

    fn collect_files(&self, indices: &mut Vec<usize>) {
        indices.extend(&self.files);
        for child in self.directories.values() {
            child.collect_files(indices);
        }
    }
}

pub struct PullRequestsPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    position: DockPosition,
    repo_dir: Option<PathBuf>,
    sections: HashMap<Section, LoadState<Vec<PullRequestSummary>>>,
    expanded_sections: HashSet<Section>,
    /// Keyed by section too, so a pull request listed in two sections expands
    /// independently in each, like tree nodes in VS Code.
    expanded_pull_requests: HashSet<(Section, u32)>,
    files: HashMap<u32, LoadState<PullRequestFiles>>,
    collapsed_directories: HashSet<(Section, u32, String)>,
    rows: Vec<Row>,
    selected: Option<usize>,
    _subscriptions: Vec<Subscription>,
}

impl PullRequestsPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.register_action(|workspace, _: &TogglePullRequestsPanel, window, cx| {
                workspace.toggle_panel_focus::<PullRequestsPanel>(window, cx);
            });
            let project = workspace.project().clone();
            let workspace = cx.weak_entity();
            cx.new(|cx| Self::new(workspace, project, window, cx))
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let git_store = project.read(cx).git_store().clone();
        let subscription = cx.subscribe(&git_store, |this, _, event, cx| {
            if let GitStoreEvent::ActiveRepositoryChanged(_) = event {
                this.active_repository_changed(cx);
            }
        });
        let repo_dir = active_repo_dir(&project, cx);
        Self {
            workspace,
            project,
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            position: DockPosition::Left,
            repo_dir,
            sections: HashMap::default(),
            expanded_sections: HashSet::from_iter([Section::AllOpen]),
            expanded_pull_requests: HashSet::default(),
            files: HashMap::default(),
            collapsed_directories: HashSet::default(),
            rows: Vec::new(),
            selected: None,
            _subscriptions: vec![subscription],
        }
    }

    fn active_repository_changed(&mut self, cx: &mut Context<Self>) {
        let repo_dir = active_repo_dir(&self.project, cx);
        if repo_dir == self.repo_dir {
            return;
        }
        self.repo_dir = repo_dir;
        self.sections.clear();
        self.files.clear();
        self.expanded_pull_requests.clear();
        self.collapsed_directories.clear();
        self.selected = None;
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.sections.clear();
        self.files.clear();
        cx.notify();
    }

    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.expanded_sections.clear();
        self.expanded_pull_requests.clear();
        self.selected = None;
        cx.notify();
    }

    /// Starts loading whatever an expanded node needs but doesn't have, so
    /// nothing is fetched until it's on screen.
    fn load_missing(&mut self, cx: &mut Context<Self>) {
        if self.repo_dir.is_none() {
            return;
        }
        let mut missing_sections = Vec::new();
        let mut missing_files = Vec::new();
        for section in Section::ALL {
            if !self.expanded_sections.contains(&section) {
                continue;
            }
            match self.sections.get(&section) {
                None => missing_sections.push(section),
                Some(LoadState::Loaded(pull_requests)) => {
                    for pull_request in pull_requests {
                        let number = pull_request.number;
                        if self.expanded_pull_requests.contains(&(section, number))
                            && !self.files.contains_key(&number)
                            && !missing_files.contains(&number)
                        {
                            missing_files.push(number);
                        }
                    }
                }
                Some(LoadState::Loading | LoadState::Failed(_)) => {}
            }
        }
        for section in missing_sections {
            self.load_section(section, cx);
        }
        for number in missing_files {
            self.load_files(number, cx);
        }
    }

    fn load_section(&mut self, section: Section, cx: &mut Context<Self>) {
        let Some(repo_dir) = self.repo_dir.clone() else {
            return;
        };
        self.sections.insert(section, LoadState::Loading);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let repo_dir = repo_dir.clone();
                    async move { github::list_section(&repo_dir, section).await }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.repo_dir.as_ref() != Some(&repo_dir) {
                    return;
                }
                let state = match result {
                    Ok(pull_requests) => LoadState::Loaded(pull_requests),
                    Err(error) => LoadState::Failed(format!("{error:#}").into()),
                };
                this.sections.insert(section, state);
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn load_files(&mut self, number: u32, cx: &mut Context<Self>) {
        let Some(repo_dir) = self.repo_dir.clone() else {
            return;
        };
        self.files.insert(number, LoadState::Loading);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let repo_dir = repo_dir.clone();
                    async move { github::pull_request_files(&repo_dir, number).await }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.repo_dir.as_ref() != Some(&repo_dir) {
                    return;
                }
                let state = match result {
                    Ok(files) => LoadState::Loaded(files),
                    Err(error) => LoadState::Failed(format!("{error:#}").into()),
                };
                this.files.insert(number, state);
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if !self.expanded_sections.remove(&section) {
            self.expanded_sections.insert(section);
            if let Some(LoadState::Failed(_)) = self.sections.get(&section) {
                self.sections.remove(&section);
            }
        }
        cx.notify();
    }

    fn toggle_pull_request(&mut self, section: Section, number: u32, cx: &mut Context<Self>) {
        let key = (section, number);
        if !self.expanded_pull_requests.remove(&key) {
            self.expanded_pull_requests.insert(key);
            if let Some(LoadState::Failed(_)) = self.files.get(&number) {
                self.files.remove(&number);
            }
        }
        cx.notify();
    }

    fn toggle_directory(
        &mut self,
        section: Section,
        number: u32,
        path: String,
        cx: &mut Context<Self>,
    ) {
        let key = (section, number, path);
        if !self.collapsed_directories.remove(&key) {
            self.collapsed_directories.insert(key);
        }
        cx.notify();
    }

    /// Updates the checkboxes at once and reverts the ones GitHub rejects.
    fn set_viewed(
        &mut self,
        number: u32,
        paths: Vec<String>,
        viewed: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(repo_dir) = self.repo_dir.clone() else {
            return;
        };
        let Some(LoadState::Loaded(files)) = self.files.get_mut(&number) else {
            return;
        };
        let node_id = files.node_id.clone();
        let mut changed = Vec::new();
        for file in &mut files.files {
            if file.viewed != viewed && paths.contains(&file.path) {
                file.viewed = viewed;
                changed.push(file.path.clone());
            }
        }
        if changed.is_empty() {
            return;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let results = cx
                .background_spawn({
                    let changed = changed.clone();
                    async move {
                        futures::future::join_all(changed.iter().map(|path| {
                            github::set_file_viewed(&repo_dir, &node_id, path, viewed)
                        }))
                        .await
                    }
                })
                .await;
            let mut failed = Vec::new();
            let mut last_error = None;
            for (path, result) in changed.into_iter().zip(results) {
                if let Err(error) = result {
                    failed.push(path);
                    last_error = Some(error);
                }
            }
            let Some(error) = last_error else {
                return Ok(());
            };
            this.update(cx, |this, cx| {
                if let Some(LoadState::Loaded(files)) = this.files.get_mut(&number) {
                    for file in &mut files.files {
                        if failed.contains(&file.path) {
                            file.viewed = !viewed;
                        }
                    }
                }
                this.show_message(format!("Couldn't update the viewed state: {error:#}"), cx);
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn show_message(&self, message: String, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| super::show_message(workspace, message, cx))
            .log_err();
    }

    fn review_pull_request(
        &mut self,
        number: u32,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo_dir) = self.repo_dir.clone() else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("Review #{number}: {title}"),
            None,
            &["This Window", "New Window", "Cancel"],
            cx,
        );
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            let target = match answer.await {
                Ok(0) => OpenTarget::ThisWindow,
                Ok(1) => OpenTarget::NewWindow,
                _ => return Ok(()),
            };
            workspace.update_in(cx, |workspace, window, cx| {
                super::diffs::open_pull_request(workspace, repo_dir, number, target, window, cx)
            })
        })
        .detach_and_log_err(cx);
    }

    fn open_in_new_window(&mut self, number: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo_dir) = self.repo_dir.clone() else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| {
                super::diffs::open_pull_request(
                    workspace,
                    repo_dir,
                    number,
                    OpenTarget::NewWindow,
                    window,
                    cx,
                )
            })
            .log_err();
    }

    fn open_changes(&mut self, number: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo_dir) = self.repo_dir.clone() else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| {
                super::diffs::open_pull_request_changes(workspace, repo_dir, number, window, cx)
            })
            .log_err();
    }

    fn open_file(&mut self, number: u32, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo_dir) = self.repo_dir.clone() else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| {
                super::diffs::open_pull_request_file(workspace, repo_dir, number, path, window, cx)
            })
            .log_err();
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index).cloned() else {
            return;
        };
        match row {
            Row::Section { section, .. } => self.toggle_section(section, cx),
            Row::PullRequest { pull_request, .. } => {
                self.review_pull_request(pull_request.number, &pull_request.title, window, cx)
            }
            Row::AllChanges { number } => self.open_changes(number, window, cx),
            Row::Directory {
                section,
                number,
                path,
                ..
            } => self.toggle_directory(section, number, path, cx),
            Row::File { number, file, .. } => self.open_file(number, file.path, window, cx),
            Row::Message { .. } => {}
        }
    }

    fn set_expanded(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index).cloned() else {
            return;
        };
        match row {
            Row::Section { section, .. } => self.toggle_section(section, cx),
            Row::PullRequest {
                section,
                pull_request,
                ..
            } => self.toggle_pull_request(section, pull_request.number, cx),
            Row::Directory {
                section,
                number,
                path,
                ..
            } => self.toggle_directory(section, number, path, cx),
            Row::Message { .. } | Row::AllChanges { .. } | Row::File { .. } => {}
        }
    }

    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(index);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn select_index(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.rows.len() {
            self.selected = Some(index);
            self.scroll_handle.scroll_to_item(index, ScrollStrategy::Top);
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.selected.map_or(0, |selected| selected + 1);
        self.select_index(next.min(self.rows.len().saturating_sub(1)), cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self
            .selected
            .map_or(0, |selected| selected.saturating_sub(1));
        self.select_index(previous, cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select_index(0, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select_index(self.rows.len().saturating_sub(1), cx);
    }

    fn select_child(&mut self, _: &menu::SelectChild, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = self.selected else {
            return;
        };
        match self.rows.get(selected).and_then(Row::expanded) {
            Some(false) => self.set_expanded(selected, cx),
            _ => self.select_next(&menu::SelectNext, window, cx),
        }
    }

    fn select_parent(
        &mut self,
        _: &menu::SelectParent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected) = self.selected else {
            return;
        };
        let Some(row) = self.rows.get(selected) else {
            return;
        };
        if row.expanded() == Some(true) {
            self.set_expanded(selected, cx);
            return;
        }
        let depth = row.depth();
        if let Some(parent) = self.rows[..selected]
            .iter()
            .rposition(|candidate| candidate.depth() < depth)
        {
            self.select_index(parent, cx);
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected) = self.selected {
            self.activate(selected, window, cx);
        }
    }

    fn build_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for section in Section::ALL {
            let expanded = self.expanded_sections.contains(&section);
            let state = self.sections.get(&section);
            let count = match state {
                Some(LoadState::Loaded(pull_requests)) => Some(pull_requests.len()),
                _ => None,
            };
            rows.push(Row::Section {
                section,
                expanded,
                count,
            });
            if !expanded {
                continue;
            }
            match state {
                None | Some(LoadState::Loading) => rows.push(Row::loading(1)),
                Some(LoadState::Failed(error)) => rows.push(Row::Message {
                    depth: 1,
                    text: error.clone(),
                    is_error: true,
                }),
                Some(LoadState::Loaded(pull_requests)) if pull_requests.is_empty() => {
                    rows.push(Row::Message {
                        depth: 1,
                        text: "No pull requests".into(),
                        is_error: false,
                    })
                }
                Some(LoadState::Loaded(pull_requests)) => {
                    for pull_request in pull_requests {
                        let number = pull_request.number;
                        let expanded = self.expanded_pull_requests.contains(&(section, number));
                        rows.push(Row::PullRequest {
                            section,
                            pull_request: pull_request.clone(),
                            expanded,
                        });
                        if expanded {
                            self.push_file_rows(section, number, &mut rows);
                        }
                    }
                }
            }
        }
        rows
    }

    fn push_file_rows(&self, section: Section, number: u32, rows: &mut Vec<Row>) {
        match self.files.get(&number) {
            None | Some(LoadState::Loading) => rows.push(Row::loading(2)),
            Some(LoadState::Failed(error)) => rows.push(Row::Message {
                depth: 2,
                text: error.clone(),
                is_error: true,
            }),
            Some(LoadState::Loaded(files)) => {
                rows.push(Row::AllChanges { number });
                let tree = FileTree::new(&files.files);
                self.push_tree_rows((section, number), &tree, &files.files, "", 2, rows);
            }
        }
    }

    fn push_tree_rows(
        &self,
        (section, number): (Section, u32),
        tree: &FileTree,
        files: &[PullRequestFile],
        prefix: &str,
        depth: usize,
        rows: &mut Vec<Row>,
    ) {
        for (name, child) in &tree.directories {
            let mut name = name.clone();
            let mut path = join_path(prefix, &name);
            let mut node = child;
            while node.files.is_empty() && node.directories.len() == 1 {
                let Some((child_name, grandchild)) = node.directories.iter().next() else {
                    break;
                };
                name = format!("{name}/{child_name}");
                path = join_path(&path, child_name);
                node = grandchild;
            }

            let mut indices = Vec::new();
            node.collect_files(&mut indices);
            let viewed_count = indices.iter().filter(|&&index| files[index].viewed).count();
            let viewed = if viewed_count == 0 {
                ToggleState::Unselected
            } else if viewed_count == indices.len() {
                ToggleState::Selected
            } else {
                ToggleState::Indeterminate
            };
            let expanded = !self
                .collapsed_directories
                .contains(&(section, number, path.clone()));
            rows.push(Row::Directory {
                section,
                number,
                path: path.clone(),
                name,
                depth,
                expanded,
                viewed,
                file_paths: indices
                    .iter()
                    .map(|&index| files[index].path.clone())
                    .collect(),
            });
            if expanded {
                self.push_tree_rows((section, number), node, files, &path, depth + 1, rows);
            }
        }
        for &index in &tree.files {
            let file = &files[index];
            let name = file
                .path
                .rsplit_once('/')
                .map_or(file.path.as_str(), |(_, name)| name)
                .to_string();
            rows.push(Row::File {
                number,
                file: file.clone(),
                name,
                depth,
            });
        }
    }

    fn dispatch_context(&self) -> KeyContext {
        let mut context = KeyContext::default();
        context.add("PullRequestsPanel");
        context.add("menu");
        context
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_repository = self.repo_dir.is_some();
        let repo_dir = self.repo_dir.clone();
        h_flex()
            .flex_none()
            .h(rems(1.75))
            .pl_2()
            .pr_1()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new("Pull Requests")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                panel_icon_button("refresh-pull-requests", IconName::RotateCw)
                    .disabled(!has_repository)
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .child(
                panel_icon_button("collapse-pull-requests", IconName::ListCollapse)
                    .tooltip(Tooltip::text("Collapse All"))
                    .on_click(cx.listener(|this, _, _, cx| this.collapse_all(cx))),
            )
            .child(
                PopoverMenu::new("pull-requests-menu")
                    .trigger_with_tooltip(
                        panel_icon_button("pull-requests-menu-trigger", IconName::Ellipsis)
                            .disabled(!has_repository),
                        Tooltip::text("More Actions"),
                    )
                    .menu(move |window, cx| {
                        let repo_dir = repo_dir.clone()?;
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            menu.entry("Open on GitHub", None, move |_, cx| {
                                let repo_dir = repo_dir.clone();
                                cx.background_spawn(async move {
                                    github::open_repository_on_github(&repo_dir).await
                                })
                                .detach_and_log_err(cx);
                            })
                        }))
                    })
                    .anchor(Anchor::TopRight),
            )
    }

    fn row_container(
        &self,
        id: impl Into<ElementId>,
        index: usize,
        depth: usize,
        window: &Window,
        cx: &App,
    ) -> Stateful<Div> {
        let colors = cx.theme().colors();
        let selected = self.selected == Some(index);
        let focused = selected && self.focus_handle.is_focused(window);
        h_flex()
            .id(id)
            .group(ROW_GROUP)
            .relative()
            .flex_none()
            .h(row_height())
            .w_full()
            .pl(px(4. + depth as f32 * INDENT_STEP))
            .pr_1()
            .gap_1()
            .border_1()
            .border_color(if focused {
                colors.panel_focused_border
            } else {
                gpui::transparent_black()
            })
            .when(selected, |row| row.bg(colors.ghost_element_selected))
            .hover(|style| style.bg(colors.ghost_element_hover))
            .cursor_pointer()
    }

    fn render_row(&self, index: usize, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(index) else {
            return div().into_any_element();
        };
        let depth = row.depth();
        let container = self
            .row_container(("pull-requests-row", index), index, depth, window, cx)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select(index, window, cx);
                this.activate(index, window, cx);
            }));
        match row {
            Row::Section {
                section,
                expanded,
                count,
            } => container
                .child(chevron(*expanded))
                .child(
                    Label::new(section.title())
                        .size(LabelSize::Small)
                        .weight(FontWeight::MEDIUM)
                        .truncate(),
                )
                .children(count.map(|count| {
                    Label::new(count.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                }))
                .into_any_element(),
            Row::Message {
                text, is_error, ..
            } => {
                let tooltip_text = text.clone();
                container
                    .child(div().w(px(CHEVRON_WIDTH)).flex_none())
                    .child(
                        div().flex_1().min_w_0().overflow_hidden().child(
                            Label::new(text.clone())
                                .size(LabelSize::Small)
                                .color(if *is_error {
                                    Color::Error
                                } else {
                                    Color::Muted
                                })
                                .truncate(),
                        ),
                    )
                    .when(*is_error, |row| row.tooltip(Tooltip::text(tooltip_text)))
                    .into_any_element()
            }
            Row::PullRequest {
                section,
                pull_request,
                expanded,
            } => self.render_pull_request(container, index, *section, pull_request, *expanded, cx),
            Row::AllChanges { .. } => container
                .child(div().w(px(CHEVRON_WIDTH)).flex_none())
                .child(
                    Icon::new(IconName::Diff)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(Label::new("All Changes").size(LabelSize::Small))
                .into_any_element(),
            Row::Directory {
                path,
                name,
                expanded,
                viewed,
                file_paths,
                number,
                ..
            } => {
                let number = *number;
                let file_paths = file_paths.clone();
                let mark_viewed = *viewed != ToggleState::Selected;
                let this = cx.weak_entity();
                let folder_icon = FileIcons::get_folder_icon(*expanded, Path::new(path), cx)
                    .map(Icon::from_path)
                    .unwrap_or_else(|| Icon::new(IconName::Folder));
                container
                    .child(chevron(*expanded))
                    .child(
                        Checkbox::new(("viewed-directory", index), *viewed).on_click(
                            move |_, _, cx| {
                                this.update(cx, |this, cx| {
                                    this.set_viewed(number, file_paths.clone(), mark_viewed, cx)
                                })
                                .log_err();
                                cx.stop_propagation();
                            },
                        ),
                    )
                    .child(folder_icon.size(IconSize::Small).color(Color::Muted))
                    .child(
                        div().flex_1().min_w_0().overflow_hidden().child(
                            Label::new(name.clone())
                                .size(LabelSize::Small)
                                .truncate(),
                        ),
                    )
                    .into_any_element()
            }
            Row::File {
                number,
                file,
                name,
                ..
            } => self.render_file(container, index, *number, file, name, cx),
        }
    }

    fn render_pull_request(
        &self,
        container: Stateful<Div>,
        index: usize,
        section: Section,
        pull_request: &PullRequestSummary,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let number = pull_request.number;
        let details = pull_request_details(pull_request);
        let repo_dir = self.repo_dir.clone();
        container
            .child(
                div()
                    .id(("pull-request-disclosure", index))
                    .flex_none()
                    .child(chevron(expanded))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select(index, window, cx);
                        this.toggle_pull_request(section, number, cx);
                        cx.stop_propagation();
                    })),
            )
            .child(
                Icon::new(IconName::PullRequest)
                    .size(IconSize::Small)
                    .color(if pull_request.is_draft {
                        Color::Muted
                    } else {
                        Color::Success
                    }),
            )
            .child(
                div()
                    .id(("pull-request-title", index))
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(
                        Label::new(format!("#{number} {}", pull_request.title))
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .tooltip(Tooltip::text(details)),
            )
            .when(pull_request.is_draft, |row| {
                row.child(
                    Label::new("Draft")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .children(check_status_icon(pull_request.checks))
            .child(
                // The buttons float over the status instead of reserving room, so
                // titles keep their full width until the row is hovered.
                h_flex()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .pl_1()
                    .pr_1()
                    .bg(cx.theme().colors().panel_background)
                    .visible_on_hover(ROW_GROUP)
                    .child(
                        panel_icon_button(("open-in-new-window", index), IconName::ArrowUpRight)
                            .tooltip(Tooltip::text("Open in New Window"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_in_new_window(number, window, cx);
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        panel_icon_button(("open-pull-request-on-github", index), IconName::Github)
                            .tooltip(Tooltip::text("Open on GitHub"))
                            .on_click(move |_, _, cx| {
                                if let Some(repo_dir) = repo_dir.clone() {
                                    cx.background_spawn(async move {
                                        github::open_pull_request_on_github(&repo_dir, number).await
                                    })
                                    .detach_and_log_err(cx);
                                }
                                cx.stop_propagation();
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_file(
        &self,
        container: Stateful<Div>,
        index: usize,
        number: u32,
        file: &PullRequestFile,
        name: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let path = file.path.clone();
        let viewed = file.viewed;
        let this = cx.weak_entity();
        let file_icon = FileIcons::get_icon(Path::new(&file.path), cx)
            .map(Icon::from_path)
            .unwrap_or_else(|| Icon::new(IconName::File));
        let (letter, letter_color) = change_type_letter(file.change_type);
        container
            .child(div().w(px(CHEVRON_WIDTH)).flex_none())
            .child(
                Checkbox::new(("viewed-file", index), ToggleState::from(viewed)).on_click(
                    move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.set_viewed(number, vec![path.clone()], !viewed, cx)
                        })
                        .log_err();
                        cx.stop_propagation();
                    },
                ),
            )
            .child(file_icon.size(IconSize::Small).color(Color::Muted))
            .child(
                div().flex_1().min_w_0().overflow_hidden().child(
                    Label::new(name.to_string())
                        .size(LabelSize::Small)
                        .when(file.change_type == ChangeType::Deleted, Label::strikethrough)
                        .when(viewed, |label| label.color(Color::Muted))
                        .truncate(),
                ),
            )
            .child(DiffStat::new(
                ("pull-request-file-stat", index),
                file.additions as usize,
                file.deletions as usize,
            ))
            .child(
                div()
                    .w(px(12.))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .child(
                        Label::new(letter)
                            .size(LabelSize::Small)
                            .color(letter_color),
                    ),
            )
            .into_any_element()
    }

    fn render_empty_state(&self, message: &'static str) -> impl IntoElement {
        v_flex().size_full().p_3().child(
            Label::new(message)
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
    }
}

fn active_repo_dir(project: &Entity<Project>, cx: &App) -> Option<PathBuf> {
    let repository = project.read(cx).active_repository(cx)?;
    Some(repository.read(cx).work_directory_abs_path.to_path_buf())
}

fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn chevron(expanded: bool) -> impl IntoElement {
    div()
        .w(px(CHEVRON_WIDTH))
        .flex_none()
        .flex()
        .justify_center()
        .child(
            Icon::new(if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size(IconSize::Small)
            .color(Color::Muted),
        )
}

fn check_status_icon(checks: CheckStatus) -> Option<Icon> {
    let (icon, color) = match checks {
        CheckStatus::Success => (IconName::Check, Color::Success),
        CheckStatus::Failure => (IconName::XCircle, Color::Error),
        CheckStatus::Pending => (IconName::Circle, Color::Warning),
        CheckStatus::None => return None,
    };
    Some(Icon::new(icon).size(IconSize::Small).color(color))
}

fn change_type_letter(change_type: ChangeType) -> (&'static str, Color) {
    match change_type {
        ChangeType::Added => ("A", Color::VersionControlAdded),
        ChangeType::Deleted => ("D", Color::VersionControlDeleted),
        ChangeType::Renamed => ("R", Color::Accent),
        ChangeType::Modified => ("M", Color::VersionControlModified),
    }
}

fn pull_request_details(pull_request: &PullRequestSummary) -> String {
    let mut details = format!(
        "{}\nby {} · {}",
        pull_request.title, pull_request.author, pull_request.head_ref_name
    );
    let decision = match pull_request.review_decision.as_deref() {
        Some("APPROVED") => Some("Approved"),
        Some("CHANGES_REQUESTED") => Some("Changes requested"),
        Some("REVIEW_REQUIRED") => Some("Review required"),
        _ => None,
    };
    if let Some(decision) = decision {
        details.push_str(" · ");
        details.push_str(decision);
    }
    details
}

impl EventEmitter<PanelEvent> for PullRequestsPanel {}

impl Focusable for PullRequestsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PullRequestsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.load_missing(cx);
        self.rows = self.build_rows();
        if self
            .selected
            .is_some_and(|selected| selected >= self.rows.len())
        {
            self.selected = None;
        }

        v_flex()
            .id("pull-requests-panel")
            .key_context(self.dispatch_context())
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::select_child))
            .on_action(cx.listener(Self::select_parent))
            .on_action(cx.listener(Self::confirm))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .map(|panel| {
                if self.repo_dir.is_none() {
                    panel.child(self.render_empty_state(
                        "Open a folder with a GitHub repository to see its pull requests.",
                    ))
                } else {
                    panel.child(
                        uniform_list(
                            "pull-requests",
                            self.rows.len(),
                            cx.processor(|this, range: Range<usize>, window, cx| {
                                range
                                    .map(|index| this.render_row(index, window, cx))
                                    .collect()
                            }),
                        )
                        .size_full()
                        .flex_1()
                        .track_scroll(&self.scroll_handle),
                    )
                }
            })
            .on_click(cx.listener(|this, _, window, cx| {
                window.focus(&this.focus_handle, cx);
            }))
    }
}

impl Panel for PullRequestsPanel {
    fn persistent_name() -> &'static str {
        "PullRequestsPanel"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(300.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::PullRequest)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Pull Requests")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(TogglePullRequestsPanel)
    }

    fn activation_priority(&self) -> u32 {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, viewed: bool) -> PullRequestFile {
        PullRequestFile {
            path: path.into(),
            additions: 1,
            deletions: 0,
            change_type: ChangeType::Modified,
            viewed,
        }
    }

    #[test]
    fn test_file_tree_compacts_single_child_folders() {
        let files = vec![
            file("README.md", false),
            file("crates/git_ui/src/a.rs", true),
            file("crates/git_ui/src/b.rs", false),
            file("crates/zed/main.rs", false),
        ];
        let tree = FileTree::new(&files);
        assert_eq!(tree.files, vec![0]);
        let crates = &tree.directories["crates"];
        assert!(crates.files.is_empty());
        assert_eq!(crates.directories.len(), 2);
        let mut indices = Vec::new();
        crates.collect_files(&mut indices);
        assert_eq!(indices, vec![1, 2, 3]);
    }
}
