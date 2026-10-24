//! Opening a pull request's changes as diff tabs in the current window.
//!
//! The new side of each diff is the review copy's own file, so inline comments
//! work as in a review window. The old side is the file at the merge base,
//! written under `<reviews dir>/.base/<copy name>/`.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use gpui::{AsyncWindowContext, Entity, SharedString, WeakEntity};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{OpenMode, OpenOptions, SaveIntent, Workspace, notifications::NotifyTaskExt as _};

use super::{
    OpenTarget, OpenedReviewCopy, ReviewCopy, existing_worktree_for_branch, git, main_repository,
    overview, prepare_review_copy, remember_opened_copy, show_message,
};
use crate::multi_diff_view::{MultiDiffOptions, MultiDiffView};

/// How many leading bytes are checked for a NUL byte, like git's binary check.
const BINARY_CHECK_LEN: usize = 8000;

/// Opens pull request `pr_number` of the repository at `repo_dir`: its
/// overview and changes, in this window or in a new review window.
pub(crate) fn open_pull_request(
    workspace: &mut Workspace,
    repo_dir: PathBuf,
    pr_number: u32,
    target: OpenTarget,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    show_message(workspace, format!("Preparing #{pr_number}…"), cx);
    let app_state = workspace.app_state().clone();
    let workspace_handle = cx.weak_entity();
    cx.spawn_in(window, {
        let workspace_handle = workspace_handle.clone();
        async move |_, cx| {
            let opened = cx
                .background_spawn(async move { prepare_review_copy(&repo_dir, pr_number).await })
                .await?;
            match target {
                OpenTarget::NewWindow => open_in_new_window(opened, app_state, cx).await,
                OpenTarget::ThisWindow => open_in_this_window(opened, workspace_handle, cx).await,
            }
        }
    })
    .detach_and_notify_err(workspace_handle, window, cx);
}

/// Opens one changed file of the pull request as a diff tab in this window.
pub(crate) fn open_pull_request_file(
    workspace: &mut Workspace,
    repo_dir: PathBuf,
    pr_number: u32,
    path: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let file_name = path.rsplit('/').next().unwrap_or(&path).to_string();
    let title = SharedString::from(format!("{file_name} · #{pr_number}"));
    open_diff(
        workspace,
        repo_dir,
        pr_number,
        Some(path),
        title,
        window,
        cx,
    );
}

/// Opens every changed file of the pull request in one diff tab in this window.
pub(crate) fn open_pull_request_changes(
    workspace: &mut Workspace,
    repo_dir: PathBuf,
    pr_number: u32,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let title = changes_title(pr_number);
    open_diff(workspace, repo_dir, pr_number, None, title, window, cx);
}

fn changes_title(pr_number: u32) -> SharedString {
    format!("PR #{pr_number} changes").into()
}

/// Shows the diff tab titled `title`, reusing an open one, or else prepares
/// the files of `only` (or all changed files) and opens a new tab.
fn open_diff(
    workspace: &mut Workspace,
    repo_dir: PathBuf,
    pr_number: u32,
    only: Option<String>,
    title: SharedString,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(existing) = diff_tab(workspace, &title, cx) {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let workspace_handle = cx.weak_entity();
    cx.spawn_in(window, {
        let workspace_handle = workspace_handle.clone();
        async move |_, cx| {
            let copy = cx
                .background_spawn(async move { ensure_review_copy(&repo_dir, pr_number).await })
                .await?;
            let diff = cx
                .background_spawn({
                    let copy = copy.clone();
                    async move { prepare_diff(&copy, only.as_deref()).await }
                })
                .await?;
            let open = workspace_handle.update_in(cx, |workspace, window, cx| {
                remember_opened_copy(workspace, copy, cx);
                open_diff_tab(workspace, diff, title, window, cx)
            })?;
            open.await?;
            anyhow::Ok(())
        }
    })
    .detach_and_notify_err(workspace_handle, window, cx);
}

async fn open_in_new_window(
    opened: OpenedReviewCopy,
    app_state: std::sync::Arc<workspace::AppState>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let open = cx.update(|_, cx| {
        workspace::open_paths(
            std::slice::from_ref(&opened.copy.dir),
            app_state,
            OpenOptions {
                open_mode: OpenMode::NewWindow,
                ..Default::default()
            },
            cx,
        )
    })?;
    let result = open.await?;
    let OpenedReviewCopy {
        copy,
        kept_local_edits,
    } = opened;
    result.window.update(cx, |_, window, cx| {
        result.workspace.update(cx, |workspace, cx| {
            workspace.open_panel::<crate::git_panel::GitPanel>(window, cx);
            overview::open_overview(workspace, copy, window, cx);
            if kept_local_edits {
                show_kept_local_edits(workspace, cx);
            }
        })
    })?;
    Ok(())
}

async fn open_in_this_window(
    opened: OpenedReviewCopy,
    workspace: WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let OpenedReviewCopy {
        copy,
        kept_local_edits,
    } = opened;
    let diff = cx
        .background_spawn({
            let copy = copy.clone();
            async move { prepare_diff(&copy, None).await }
        })
        .await?;
    let title = changes_title(copy.pr_number);
    let (open, previous_tab) = workspace.update_in(cx, |workspace, window, cx| {
        remember_opened_copy(workspace, copy.clone(), cx);
        overview::open_overview(workspace, copy, window, cx);
        if kept_local_edits {
            show_kept_local_edits(workspace, cx);
        }
        let previous_tab = diff_tab(workspace, &title, cx);
        (
            open_diff_tab(workspace, diff, title, window, cx),
            previous_tab,
        )
    })?;
    open.await?;
    // The pull request may have changed since that tab was opened, so it's
    // replaced, after the new one is in place to keep the tab bar steady.
    if let Some(previous_tab) = previous_tab {
        workspace.update_in(cx, |workspace, window, cx| {
            if let Some(pane) = workspace.pane_for(&previous_tab) {
                pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(previous_tab.entity_id(), SaveIntent::Skip, window, cx)
                })
                .detach_and_log_err(cx);
            }
        })?;
    }
    Ok(())
}

fn show_kept_local_edits(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    show_message(
        workspace,
        "The pull request has new commits, but its review copy has your edits, so it was not refreshed.",
        cx,
    );
}

/// The open diff tab titled `title`, if any.
fn diff_tab(
    workspace: &Workspace,
    title: &SharedString,
    cx: &App,
) -> Option<Entity<MultiDiffView>> {
    workspace
        .items_of_type::<MultiDiffView>(cx)
        .find(|view| view.read(cx).title_override() == Some(title))
}

fn open_diff_tab(
    workspace: &mut Workspace,
    diff: PullRequestDiff,
    title: SharedString,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> gpui::Task<Result<Entity<MultiDiffView>>> {
    if diff.skipped_binary > 0 {
        let files = if diff.skipped_binary == 1 {
            "1 binary file".to_string()
        } else {
            format!("{} binary files", diff.skipped_binary)
        };
        show_message(workspace, format!("Not showing {files}."), cx);
    }
    if diff.pairs.is_empty() {
        return gpui::Task::ready(Err(anyhow!(
            "this pull request has no text changes to show"
        )));
    }
    // Without a worktree rooted at the review copy, the copy's files sit in
    // single-file worktrees and their headers show the full path on disk.
    let add_worktree = workspace.project().update(cx, |project, cx| {
        project.find_or_create_worktree(&diff.copy_dir, false, cx)
    });
    cx.spawn_in(window, async move |workspace, cx| {
        add_worktree.await?;
        workspace
            .update_in(cx, |workspace, window, cx| {
                MultiDiffView::open_with_options(
                    diff.pairs,
                    MultiDiffOptions {
                        title: Some(title),
                        keep_left_dock_open: true,
                        display_paths: Some(diff.display_paths),
                    },
                    workspace,
                    window,
                    cx,
                )
            })?
            .await
    })
}

/// The pull request's review copy, created when there is none yet.
async fn ensure_review_copy(repo_dir: &Path, pr_number: u32) -> Result<ReviewCopy> {
    let repo = main_repository(repo_dir).await?;
    let repo_name = repo
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("unexpected repository path {}", repo.display()))?;
    let copy = ReviewCopy::new(repo_name, pr_number);
    let existing_dir = existing_worktree_for_branch(&repo, &copy.branch()).await?;
    let has_head = git(
        &repo,
        &["rev-parse", "--quiet", "--verify", &copy.head_ref()],
    )
    .await
    .is_ok();
    if let Some(dir) = existing_dir
        && has_head
        && smol::fs::metadata(&dir).await.is_ok()
    {
        return Ok(ReviewCopy { dir, ..copy });
    }
    Ok(prepare_review_copy(&repo, pr_number).await?.copy)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ChangeKind {
    Added,
    Modified,
    Deleted,
    /// Renamed or copied from another path.
    Moved {
        from: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ChangedFile {
    path: String,
    kind: ChangeKind,
}

/// Parses the output of `git diff --name-status -z`.
fn parse_name_status(output: &str) -> Vec<ChangedFile> {
    let mut fields = output.split('\0').filter(|field| !field.is_empty());
    let mut files = Vec::new();
    while let Some(status) = fields.next() {
        let Some(path) = fields.next() else {
            break;
        };
        let file = match status.chars().next() {
            Some('R' | 'C') => {
                let Some(to) = fields.next() else {
                    break;
                };
                ChangedFile {
                    path: to.to_string(),
                    kind: ChangeKind::Moved {
                        from: path.to_string(),
                    },
                }
            }
            Some('A') => ChangedFile {
                path: path.to_string(),
                kind: ChangeKind::Added,
            },
            Some('D') => ChangedFile {
                path: path.to_string(),
                kind: ChangeKind::Deleted,
            },
            _ => ChangedFile {
                path: path.to_string(),
                kind: ChangeKind::Modified,
            },
        };
        files.push(file);
    }
    files
}

/// The files to diff, as `[old, new]` absolute paths with the repository path
/// to show for each.
struct PullRequestDiff {
    copy_dir: PathBuf,
    pairs: Vec<[String; 2]>,
    display_paths: Vec<String>,
    skipped_binary: usize,
}

/// Writes the merge base side of the PR's changed files (or just `only`) and
/// pairs each with its file in the review copy.
async fn prepare_diff(copy: &ReviewCopy, only: Option<&str>) -> Result<PullRequestDiff> {
    let merge_base = git(&copy.dir, &["rev-parse", "HEAD"]).await?;
    let output = git(
        &copy.dir,
        &[
            "diff",
            "--name-status",
            "-z",
            "--find-renames",
            &merge_base,
            &copy.head_ref(),
        ],
    )
    .await?;
    let mut files = parse_name_status(&output);
    if let Some(only) = only {
        files.retain(|file| file.path == only);
        if files.is_empty() {
            anyhow::bail!("{only} is not changed in pull request #{}", copy.pr_number);
        }
    }

    let base_dir = copy.base_files_dir();
    let deleted_dir = copy.deleted_files_dir();
    let mut diff = PullRequestDiff {
        copy_dir: copy.dir.clone(),
        pairs: Vec::new(),
        display_paths: Vec::new(),
        skipped_binary: 0,
    };
    for file in files {
        let base_source = match &file.kind {
            ChangeKind::Added => None,
            ChangeKind::Modified | ChangeKind::Deleted => Some(file.path.as_str()),
            ChangeKind::Moved { from } => Some(from.as_str()),
        };
        let base_contents = match base_source {
            Some(source) => git_show(&copy.dir, &format!("{merge_base}:{source}")).await?,
            None => Vec::new(),
        };
        let base_path = base_dir.join(base_source.unwrap_or(&file.path));

        let new_path = if file.kind == ChangeKind::Deleted {
            let placeholder = deleted_dir.join(&file.path);
            write_if_changed(&placeholder, &[]).await?;
            placeholder
        } else {
            copy.dir.join(&file.path)
        };
        // Submodules show up as directories; there's no text to compare.
        if smol::fs::metadata(&new_path)
            .await
            .is_ok_and(|metadata| metadata.is_dir())
        {
            continue;
        }
        let new_contents = smol::fs::read(&new_path)
            .await
            .with_context(|| format!("reading {}", new_path.display()))
            .log_err()
            .unwrap_or_default();
        if is_binary(&base_contents) || is_binary(&new_contents) {
            diff.skipped_binary += 1;
            continue;
        }
        write_if_changed(&base_path, &base_contents).await?;

        diff.pairs.push([
            base_path.to_string_lossy().into_owned(),
            new_path.to_string_lossy().into_owned(),
        ]);
        diff.display_paths.push(file.path);
    }
    Ok(diff)
}

fn is_binary(contents: &[u8]) -> bool {
    contents[..contents.len().min(BINARY_CHECK_LEN)].contains(&0)
}

/// The raw bytes of a `git show <spec>`, unlike `git`, which trims its output.
async fn git_show(dir: &Path, spec: &str) -> Result<Vec<u8>> {
    let output = util::command::new_command("git")
        .args(["show", spec])
        .current_dir(dir)
        .output()
        .await
        .context("running git; is it installed and on PATH?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git show {spec}: {}", stderr.trim());
    }
    Ok(output.stdout)
}

/// Writes the file unless it already has these contents, so open buffers of
/// it don't reload for nothing.
async fn write_if_changed(path: &Path, contents: &[u8]) -> Result<()> {
    if smol::fs::read(path)
        .await
        .is_ok_and(|existing| existing == contents)
    {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        smol::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    smol::fs::write(path, contents)
        .await
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_name_status() {
        let output =
            "M\0src/main.rs\0A\0new file.txt\0D\0old.rs\0R087\0a/from.rs\0b/to.rs\0T\0link\0";
        assert_eq!(
            parse_name_status(output),
            vec![
                ChangedFile {
                    path: "src/main.rs".into(),
                    kind: ChangeKind::Modified,
                },
                ChangedFile {
                    path: "new file.txt".into(),
                    kind: ChangeKind::Added,
                },
                ChangedFile {
                    path: "old.rs".into(),
                    kind: ChangeKind::Deleted,
                },
                ChangedFile {
                    path: "b/to.rs".into(),
                    kind: ChangeKind::Moved {
                        from: "a/from.rs".into(),
                    },
                },
                ChangedFile {
                    path: "link".into(),
                    kind: ChangeKind::Modified,
                },
            ]
        );
        assert!(parse_name_status("").is_empty());
        // A truncated rename entry is dropped rather than misread.
        assert!(parse_name_status("R100\0only-from.rs\0").is_empty());
    }

    #[test]
    fn test_is_binary() {
        assert!(!is_binary(b"fn main() {}\n"));
        assert!(is_binary(b"\x89PNG\0\0"));
        assert!(!is_binary(b""));
    }
}
