//! A read-only project item for binary files, such as spreadsheets and Word documents, that are
//! shown by a viewer instead of a text editor.
//!
//! [`BinaryFileItem`] reads its file from a local worktree in the background, parses it with a
//! [`FileFormat`] off the main thread, and reads it again whenever the worktree reports that the
//! file changed on disk.

use std::{marker::PhantomData, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, SharedString, Subscription, Task,
};
use project::{MTime, Project, ProjectEntryId, ProjectPath, UpdatedEntriesSet, Worktree};
use util::ResultExt as _;

/// A file format that a [`BinaryFileItem`] can open.
pub trait FileFormat: 'static {
    /// The parsed file, shared between the item and its views.
    type Content: Send + Sync + 'static;

    /// Lowercase file extensions, without the leading dot, opened with this format.
    const EXTENSIONS: &'static [&'static str];

    /// What the file is called in messages, e.g. "Spreadsheet".
    const DISPLAY_NAME: &'static str;

    /// Parses the bytes of the file. Runs on a background thread.
    fn parse(bytes: &[u8]) -> Result<Self::Content>;
}

pub enum LoadState<C> {
    Loading,
    Loaded(Arc<C>),
    Failed(SharedString),
}

impl<C> Clone for LoadState<C> {
    fn clone(&self) -> Self {
        match self {
            Self::Loading => Self::Loading,
            Self::Loaded(content) => Self::Loaded(content.clone()),
            Self::Failed(error) => Self::Failed(error.clone()),
        }
    }
}

pub enum BinaryFileItemEvent {
    /// The file finished loading, or failed to load.
    ContentChanged,
    /// The file was renamed, deleted or restored.
    FileChanged,
}

pub struct BinaryFileItem<F: FileFormat> {
    worktree: Entity<Worktree>,
    project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
    loaded_mtime: Option<MTime>,
    state: LoadState<F::Content>,
    is_deleted: bool,
    load_task: Option<Task<()>>,
    _worktree_subscription: Subscription,
    _format: PhantomData<F>,
}

impl<F: FileFormat> EventEmitter<BinaryFileItemEvent> for BinaryFileItem<F> {}

impl<F: FileFormat> BinaryFileItem<F> {
    fn new(worktree: Entity<Worktree>, project_path: ProjectPath, cx: &mut Context<Self>) -> Self {
        let entry_id = worktree
            .read(cx)
            .entry_for_path(&project_path.path)
            .map(|entry| entry.id);
        let worktree_subscription =
            cx.subscribe(&worktree, |this, worktree, event, cx| match event {
                worktree::Event::UpdatedEntries(changes) => {
                    this.worktree_entries_changed(&worktree, changes, cx);
                }
                worktree::Event::DeletedEntry(entry_id) if Some(*entry_id) == this.entry_id => {
                    this.set_deleted(true, cx);
                }
                worktree::Event::Deleted => this.set_deleted(true, cx),
                _ => {}
            });

        let mut this = Self {
            worktree,
            project_path,
            entry_id,
            loaded_mtime: None,
            state: LoadState::Loading,
            is_deleted: false,
            load_task: None,
            _worktree_subscription: worktree_subscription,
            _format: PhantomData,
        };
        this.reload(cx);
        this
    }

    /// Opens the file at `abs_path`, adding a worktree for it if needed. Used to restore views
    /// from a previous session, where only the absolute path was saved.
    pub async fn open_abs_path(
        project: Entity<Project>,
        abs_path: PathBuf,
        cx: &mut AsyncApp,
    ) -> Result<Entity<Self>> {
        let (worktree, relative_path) = project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(abs_path, false, cx)
            })
            .await
            .context("Path not found")?;
        let project_path = ProjectPath {
            worktree_id: worktree.read_with(cx, |worktree, _| worktree.id()),
            path: relative_path,
        };
        let open_item =
            cx.update(|cx| <Self as project::ProjectItem>::try_open(&project, &project_path, cx));
        open_item
            .with_context(|| format!("Not a {} file", F::DISPLAY_NAME.to_lowercase()))?
            .await
    }

    pub fn state(&self) -> &LoadState<F::Content> {
        &self.state
    }

    pub fn is_deleted(&self) -> bool {
        self.is_deleted
    }

    pub fn abs_path(&self, cx: &App) -> PathBuf {
        self.worktree.read(cx).absolutize(&self.project_path.path)
    }

    pub fn file_name(&self, cx: &App) -> SharedString {
        // Files opened on their own get a single-file worktree whose relative path is empty, so
        // the name has to come from the absolute path.
        self.abs_path(cx)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned().into())
            .unwrap_or_else(|| F::DISPLAY_NAME.into())
    }

    fn worktree_entries_changed(
        &mut self,
        worktree: &Entity<Worktree>,
        changes: &UpdatedEntriesSet,
        cx: &mut Context<Self>,
    ) {
        let is_relevant = changes.iter().any(|(path, entry_id, _)| {
            Some(*entry_id) == self.entry_id || *path == self.project_path.path
        });
        if !is_relevant {
            return;
        }

        // Saving through a temporary file and a rename gives the file a new entry id, so fall
        // back to looking the entry up by path.
        let snapshot = worktree.read(cx);
        let entry = self
            .entry_id
            .and_then(|entry_id| snapshot.entry_for_id(entry_id))
            .or_else(|| snapshot.entry_for_path(&self.project_path.path))
            .map(|entry| (entry.id, entry.path.clone(), entry.mtime));

        let Some((entry_id, path, mtime)) = entry else {
            self.set_deleted(true, cx);
            return;
        };

        self.entry_id = Some(entry_id);
        if path != self.project_path.path {
            self.project_path.path = path;
            cx.emit(BinaryFileItemEvent::FileChanged);
        }
        self.set_deleted(false, cx);
        if mtime != self.loaded_mtime {
            self.reload(cx);
        }
    }

    fn set_deleted(&mut self, is_deleted: bool, cx: &mut Context<Self>) {
        if self.is_deleted != is_deleted {
            self.is_deleted = is_deleted;
            cx.emit(BinaryFileItemEvent::FileChanged);
            cx.notify();
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let load_file = self.worktree.update(cx, |worktree, cx| {
            worktree.load_binary_file(&self.project_path.path, cx)
        });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let loaded_file = load_file.await?;
                let mtime = loaded_file.file.disk_state.mtime();
                let bytes = loaded_file.content;
                let content = cx.background_spawn(async move { F::parse(&bytes) }).await?;
                anyhow::Ok((mtime, content))
            }
            .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok((mtime, content)) => {
                        this.loaded_mtime = mtime;
                        this.state = LoadState::Loaded(Arc::new(content));
                    }
                    Err(error) => {
                        log::error!("Failed to load {}: {error:#}", F::DISPLAY_NAME);
                        this.state = LoadState::Failed(format!("{error:#}").into());
                    }
                }
                cx.emit(BinaryFileItemEvent::ContentChanged);
                cx.notify();
            })
            .log_err();
        }));
    }
}

fn has_extension<F: FileFormat>(extension: &str) -> bool {
    F::EXTENSIONS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(extension))
}

impl<F: FileFormat> project::ProjectItem for BinaryFileItem<F> {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        // For single-file worktrees the relative path is empty, so fall back to the absolute path
        // to detect files opened directly.
        let matches_format = path.path.extension().is_some_and(has_extension::<F>)
            || project
                .read(cx)
                .absolute_path(path, cx)
                .as_deref()
                .and_then(|abs_path| abs_path.extension())
                .and_then(|extension| extension.to_str())
                .is_some_and(has_extension::<F>);
        if !matches_format {
            return None;
        }

        let worktree = project.read(cx).worktree_for_id(path.worktree_id, cx);
        let path = path.clone();
        Some(cx.spawn(async move |cx| {
            let worktree = worktree.context("Worktree not found")?;
            let is_local = worktree.read_with(cx, |worktree, _| worktree.is_local());
            anyhow::ensure!(
                is_local,
                "{} files can only be viewed in local projects",
                F::DISPLAY_NAME
            );
            Ok(cx.new(|cx| Self::new(worktree, path, cx)))
        }))
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.entry_id
    }

    fn project_path(&self, _: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Fs as _, ProjectItem as _};
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use util::{path, rel_path::rel_path};

    /// Files ending in `.upper` whose content is shown upper-cased. Content that is not UTF-8
    /// fails to parse.
    struct UpperFormat;

    impl FileFormat for UpperFormat {
        type Content = String;
        const EXTENSIONS: &'static [&'static str] = &["upper"];
        const DISPLAY_NAME: &'static str = "Upper";

        fn parse(bytes: &[u8]) -> Result<String> {
            Ok(std::str::from_utf8(bytes)?.to_uppercase())
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    async fn open(
        project: &Entity<Project>,
        relative_path: &str,
        cx: &mut TestAppContext,
    ) -> Option<Result<Entity<BinaryFileItem<UpperFormat>>>> {
        let project_path = cx.update(|cx| {
            let worktree_id = project.read(cx).worktrees(cx).next()?.read(cx).id();
            Some(ProjectPath {
                worktree_id,
                path: rel_path(relative_path).into(),
            })
        })?;
        let task =
            cx.update(|cx| BinaryFileItem::<UpperFormat>::try_open(project, &project_path, cx))?;
        Some(task.await)
    }

    fn content(item: &Entity<BinaryFileItem<UpperFormat>>, cx: &TestAppContext) -> Option<String> {
        item.read_with(cx, |item, _| match item.state() {
            LoadState::Loaded(content) => Some(content.as_ref().clone()),
            LoadState::Loading | LoadState::Failed(_) => None,
        })
    }

    #[gpui::test]
    async fn test_opens_only_matching_extensions(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ "a.upper": "hello", "b.UPPER": "loud", "c.txt": "plain" }),
        )
        .await;
        let project = Project::test(fs, [Path::new(path!("/root"))], cx).await;

        let result: Result<()> = async {
            let item = open(&project, "a.upper", cx)
                .await
                .context("a.upper should match")??;
            cx.run_until_parked();
            assert_eq!(content(&item, cx).as_deref(), Some("HELLO"));
            item.read_with(cx, |item, cx| {
                assert_eq!(item.file_name(cx), "a.upper");
                assert!(item.entry_id(cx).is_some());
            });

            let item = open(&project, "b.UPPER", cx)
                .await
                .context("extension matching ignores case")??;
            cx.run_until_parked();
            assert_eq!(content(&item, cx).as_deref(), Some("LOUD"));

            assert!(open(&project, "c.txt", cx).await.is_none());
            Ok(())
        }
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[gpui::test]
    async fn test_reloads_and_tracks_deletion(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ "a.upper": "first" }))
            .await;
        let project = Project::test(fs.clone(), [Path::new(path!("/root"))], cx).await;

        let result: Result<()> = async {
            let item = open(&project, "a.upper", cx)
                .await
                .context("a.upper should match")??;
            cx.run_until_parked();
            assert_eq!(content(&item, cx).as_deref(), Some("FIRST"));

            fs.insert_file(path!("/root/a.upper"), b"second".to_vec())
                .await;
            cx.run_until_parked();
            assert_eq!(content(&item, cx).as_deref(), Some("SECOND"));

            fs.remove_file(Path::new(path!("/root/a.upper")), Default::default())
                .await?;
            cx.run_until_parked();
            assert!(item.read_with(cx, |item, _| item.is_deleted()));

            fs.insert_file(path!("/root/a.upper"), b"third".to_vec())
                .await;
            cx.run_until_parked();
            assert!(!item.read_with(cx, |item, _| item.is_deleted()));
            assert_eq!(content(&item, cx).as_deref(), Some("THIRD"));
            Ok(())
        }
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[gpui::test]
    async fn test_reports_parse_errors(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        fs.insert_file(path!("/root/bad.upper"), vec![0xff, 0xfe])
            .await;
        let project = Project::test(fs, [Path::new(path!("/root"))], cx).await;

        let result: Result<()> = async {
            let item = open(&project, "bad.upper", cx)
                .await
                .context("bad.upper should match")??;
            cx.run_until_parked();
            item.read_with(cx, |item, _| {
                assert!(matches!(item.state(), LoadState::Failed(_)));
            });
            Ok(())
        }
        .await;
        assert!(result.is_ok(), "{result:?}");
    }
}
