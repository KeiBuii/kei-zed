mod workbook;

use std::sync::Arc;

use anyhow::{Context as _, Result};
use binary_file_item::{BinaryFileItem, BinaryFileItemEvent, FileFormat, LoadState};
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, Subscription,
    Task, WeakEntity, Window,
};
use persistence::SpreadsheetViewerDb;
use project::Project;
use settings::Settings as _;
use tabular_data_preview::TableView;
use ui::{ButtonStyle, prelude::*};
use util::{ResultExt as _, paths::PathExt as _};
use workspace::{
    ItemId, ItemSettings, Pane, Workspace, WorkspaceId, delete_unloaded_items,
    item::{Item, ItemBufferKind, ItemEvent, ProjectItem, SerializableItem, TabContentParams},
};

pub use crate::workbook::{Sheet, Workbook, column_name, parse_workbook};

pub fn init(cx: &mut App) {
    workspace::register_project_item::<SpreadsheetView>(cx);
    workspace::register_serializable_item::<SpreadsheetView>(cx);
}

pub struct SpreadsheetFormat;

impl FileFormat for SpreadsheetFormat {
    type Content = Workbook;
    const EXTENSIONS: &'static [&'static str] = &["xlsx", "xlsm", "xls", "ods"];
    const DISPLAY_NAME: &'static str = "Spreadsheet";

    fn parse(bytes: &[u8]) -> Result<Workbook> {
        parse_workbook(bytes)
    }
}

pub type SpreadsheetItem = BinaryFileItem<SpreadsheetFormat>;

pub enum SpreadsheetViewEvent {
    TitleChanged,
}

/// A read-only tab showing one sheet of a [`SpreadsheetItem`] at a time in a [`TableView`].
pub struct SpreadsheetView {
    item: Entity<SpreadsheetItem>,
    project: Entity<Project>,
    table: Entity<TableView>,
    /// The workbook whose active sheet is in `table`. Compared with the item's workbook to tell
    /// a reload apart from other item notifications.
    shown_workbook: Option<Arc<Workbook>>,
    /// Kept by name so the same sheet stays selected after the file is reloaded.
    active_sheet_name: Option<SharedString>,
    set_contents_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SpreadsheetViewEvent> for SpreadsheetView {}

impl SpreadsheetView {
    pub fn new(
        item: Entity<SpreadsheetItem>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let table = cx.new(|cx| TableView::new(window, cx));
        let subscriptions = vec![
            cx.subscribe(&item, |this, _, event, cx| match event {
                BinaryFileItemEvent::ContentChanged => this.sync_with_item(cx),
                BinaryFileItemEvent::FileChanged => cx.emit(SpreadsheetViewEvent::TitleChanged),
            }),
            cx.observe(&table, |_, _, cx| cx.notify()),
        ];

        let mut this = Self {
            item,
            project,
            table,
            shown_workbook: None,
            active_sheet_name: None,
            set_contents_task: None,
            _subscriptions: subscriptions,
        };
        this.sync_with_item(cx);
        this
    }

    pub fn item(&self) -> &Entity<SpreadsheetItem> {
        &self.item
    }

    pub fn table(&self) -> &Entity<TableView> {
        &self.table
    }

    pub fn active_sheet_name(&self) -> Option<&SharedString> {
        self.active_sheet_name.as_ref()
    }

    fn sync_with_item(&mut self, cx: &mut Context<Self>) {
        match self.item.read(cx).state().clone() {
            LoadState::Loading => {
                self.table
                    .update(cx, |table, cx| table.set_loading(true, cx));
            }
            LoadState::Failed(_) => {
                self.shown_workbook = None;
                self.set_contents_task = None;
            }
            LoadState::Loaded(workbook) => {
                let is_shown = self
                    .shown_workbook
                    .as_ref()
                    .is_some_and(|shown| Arc::ptr_eq(shown, &workbook));
                if !is_shown {
                    let keeps_active_sheet = self
                        .active_sheet_name
                        .as_ref()
                        .is_some_and(|name| workbook.sheet_index(name).is_some());
                    if !keeps_active_sheet {
                        self.active_sheet_name =
                            workbook.sheets.first().map(|sheet| sheet.name.clone());
                    }
                    self.shown_workbook = Some(workbook);
                    self.show_active_sheet(cx);
                }
            }
        }
        cx.notify();
    }

    pub fn select_sheet(&mut self, name: SharedString, cx: &mut Context<Self>) {
        if self.active_sheet_name.as_ref() == Some(&name) {
            return;
        }
        self.active_sheet_name = Some(name);
        self.show_active_sheet(cx);
        cx.notify();
    }

    fn active_sheet(&self) -> Option<&Sheet> {
        let workbook = self.shown_workbook.as_ref()?;
        let index = workbook.sheet_index(self.active_sheet_name.as_ref()?)?;
        workbook.sheets.get(index)
    }

    fn show_active_sheet(&mut self, cx: &mut Context<Self>) {
        let Some((workbook, name)) = self
            .shown_workbook
            .clone()
            .zip(self.active_sheet_name.clone())
        else {
            return;
        };

        self.table
            .update(cx, |table, cx| table.set_loading(true, cx));
        // Copying a large sheet takes a while, so it happens off the main thread.
        let content = cx.background_spawn(async move {
            let index = workbook.sheet_index(&name)?;
            workbook
                .sheets
                .get(index)
                .map(|sheet| sheet.content.clone())
        });
        let table = self.table.downgrade();
        self.set_contents_task = Some(cx.spawn(async move |_, cx| {
            if let Some(content) = content.await {
                table
                    .update(cx, |table, cx| table.set_contents(content, cx))
                    .log_err();
            }
        }));
    }

    fn render_sheet_tabs(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let workbook = self.shown_workbook.as_ref()?;
        let tabs = workbook.sheets.iter().enumerate().map(|(index, sheet)| {
            let name = sheet.name.clone();
            Button::new(("spreadsheet-sheet", index), sheet.name.clone())
                .style(ButtonStyle::Subtle)
                .toggle_state(self.active_sheet_name.as_ref() == Some(&sheet.name))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_sheet(name.clone(), cx);
                }))
        });

        Some(
            h_flex()
                .id("spreadsheet-sheet-tabs")
                .w_full()
                .flex_none()
                .gap_1()
                .p_1()
                .overflow_x_scroll()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().editor_background)
                .children(tabs)
                .into_any_element(),
        )
    }
}

fn render_message(message: SharedString, color: Color) -> AnyElement {
    h_flex()
        .size_full()
        .justify_center()
        .p_4()
        .child(Label::new(message).color(color))
        .into_any_element()
}

impl Render for SpreadsheetView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.item.read(cx).state() {
            LoadState::Failed(error) => render_message(error.clone(), Color::Error),
            LoadState::Loaded(_)
                if self
                    .active_sheet()
                    .is_none_or(|sheet| sheet.content.number_of_cols == 0) =>
            {
                render_message("This sheet is empty".into(), Color::Muted)
            }
            LoadState::Loading | LoadState::Loaded(_) => self.table.clone().into_any_element(),
        };

        v_flex()
            .size_full()
            .child(div().flex_1().min_h_0().w_full().child(body))
            .children(self.render_sheet_tabs(cx))
    }
}

impl Focusable for SpreadsheetView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.read(cx).focus_handle(cx)
    }
}

impl Item for SpreadsheetView {
    type Event = SpreadsheetViewEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            SpreadsheetViewEvent::TitleChanged => f(ItemEvent::UpdateTab),
        }
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.item.entity_id(), self.item.read(cx))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(params.text_color())
            .when(params.preview, |label| label.italic())
            .into_any_element()
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.item.read(cx).file_name(cx)
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let abs_path = self.item.read(cx).abs_path(cx);
        Some(abs_path.compact().to_string_lossy().into_owned().into())
    }

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        let abs_path = self.item.read(cx).abs_path(cx);
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| FileIcons::get_icon(&abs_path, cx))
            .flatten()
            .map(Icon::from_path)
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let item = self.item.clone();
        let project = self.project.clone();
        Task::ready(Some(
            cx.new(|cx| SpreadsheetView::new(item, project, window, cx)),
        ))
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.item.read(cx).is_deleted()
    }

    fn buffer_kind(&self, _: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }
}

impl ProjectItem for SpreadsheetView {
    type Item = SpreadsheetItem;

    fn for_project_item(
        project: Entity<Project>,
        _pane: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(item, project, window, cx)
    }
}

impl SerializableItem for SpreadsheetView {
    fn serialized_item_kind() -> &'static str {
        "SpreadsheetView"
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = SpreadsheetViewerDb::global(cx);
        window.spawn(cx, async move |cx| {
            let abs_path = db
                .get_spreadsheet_path(item_id, workspace_id)?
                .context("No spreadsheet path found")?;

            let item = SpreadsheetItem::open_abs_path(project.clone(), abs_path, cx).await?;
            cx.update(|window, cx| cx.new(|cx| SpreadsheetView::new(item, project, window, cx)))
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let db = SpreadsheetViewerDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "spreadsheet_viewers", &db, cx)
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let abs_path = self.item.read(cx).abs_path(cx);
        let db = SpreadsheetViewerDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_spreadsheet_path(item_id, workspace_id, abs_path)
                .await
        }))
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}

mod persistence {
    use std::path::PathBuf;

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct SpreadsheetViewerDb(ThreadSafeConnection);

    impl Domain for SpreadsheetViewerDb {
        const NAME: &str = stringify!(SpreadsheetViewerDb);

        const MIGRATIONS: &[&str] = &[sql!(
                CREATE TABLE spreadsheet_viewers (
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,

                    spreadsheet_path BLOB,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
        )];
    }

    db::static_connection!(SpreadsheetViewerDb, [WorkspaceDb]);

    impl SpreadsheetViewerDb {
        query! {
            pub async fn save_spreadsheet_path(
                item_id: ItemId,
                workspace_id: WorkspaceId,
                spreadsheet_path: PathBuf
            ) -> Result<()> {
                INSERT OR REPLACE INTO spreadsheet_viewers(item_id, workspace_id, spreadsheet_path)
                VALUES (?, ?, ?)
            }
        }

        query! {
            pub fn get_spreadsheet_path(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<PathBuf>> {
                SELECT spreadsheet_path
                FROM spreadsheet_viewers
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use util::path;
    use workspace::{AppState, OpenOptions};

    const SAMPLE_XLSX: &[u8] = include_bytes!("../test_data/sample.xlsx");
    const UPDATED_XLSX: &[u8] = include_bytes!("../test_data/updated.xlsx");

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            init(cx);
        });
    }

    async fn open_path(
        abs_path: &str,
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Result<Box<dyn workspace::item::ItemHandle>> {
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(PathBuf::from(abs_path), OpenOptions::default(), window, cx)
            })
            .await
    }

    async fn open_spreadsheet(
        abs_path: &str,
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Result<Entity<SpreadsheetView>> {
        open_path(abs_path, workspace, cx)
            .await?
            .downcast::<SpreadsheetView>()
            .context("the file did not open as a spreadsheet")
    }

    /// The value in column `A` of the given row of the sheet shown in the table.
    fn shown_cell(view: &Entity<SpreadsheetView>, row: usize, cx: &VisualTestContext) -> String {
        view.read_with(cx, |view, cx| {
            view.table()
                .read(cx)
                .contents()
                .rows
                .get(row)
                .and_then(|row| row.get(0usize))
                .and_then(|cell| cell.display_value())
                .map(ToString::to_string)
                .unwrap_or_default()
        })
    }

    #[gpui::test]
    async fn test_opens_xlsx_as_spreadsheet_view(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "notes.txt": "plain" }))
            .await;
        fs.insert_file(path!("/project/sample.xlsx"), SAMPLE_XLSX.to_vec())
            .await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        let result: Result<()> = async {
            let view = open_spreadsheet(path!("/project/sample.xlsx"), &workspace, cx).await?;
            cx.run_until_parked();

            view.read_with(cx, |view, cx| {
                assert!(matches!(view.item().read(cx).state(), LoadState::Loaded(_)));
                assert_eq!(
                    view.active_sheet_name().map(|name| name.as_ref()),
                    Some("Data")
                );
                assert_eq!(view.tab_content_text(0, cx), "sample.xlsx");
            });
            assert_eq!(shown_cell(&view, 1, cx), "Ada");

            view.update(cx, |view, cx| view.select_sheet("Offset".into(), cx));
            cx.run_until_parked();
            assert_eq!(shown_cell(&view, 0, cx), "x");

            let text_item = open_path(path!("/project/notes.txt"), &workspace, cx).await?;
            assert!(text_item.downcast::<SpreadsheetView>().is_none());
            Ok(())
        }
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[gpui::test]
    async fn test_reloads_when_file_changes_on_disk(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({})).await;
        fs.insert_file(path!("/project/sample.xlsx"), SAMPLE_XLSX.to_vec())
            .await;
        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        let result: Result<()> = async {
            let view = open_spreadsheet(path!("/project/sample.xlsx"), &workspace, cx).await?;
            cx.run_until_parked();
            view.update(cx, |view, cx| view.select_sheet("Offset".into(), cx));
            cx.run_until_parked();
            assert_eq!(shown_cell(&view, 0, cx), "x");

            fs.insert_file(path!("/project/sample.xlsx"), UPDATED_XLSX.to_vec())
                .await;
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.active_sheet_name().map(|name| name.as_ref()),
                    Some("Offset")
                );
            });
            assert_eq!(shown_cell(&view, 0, cx), "y");
            Ok(())
        }
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[gpui::test]
    async fn test_shows_error_for_invalid_spreadsheet(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({ "broken.xlsx": "not a spreadsheet" }),
        )
        .await;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        let result: Result<()> = async {
            let view = open_spreadsheet(path!("/project/broken.xlsx"), &workspace, cx).await?;
            cx.run_until_parked();
            view.read_with(cx, |view, cx| {
                assert!(matches!(view.item().read(cx).state(), LoadState::Failed(_)));
            });
            Ok(())
        }
        .await;
        assert!(result.is_ok(), "{result:?}");
    }
}
