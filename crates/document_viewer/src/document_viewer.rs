mod document;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

use anyhow::Result;
use binary_file_item::{BinaryFileItem, BinaryFileItemEvent, FileFormat, LoadState};
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Image, ImageFormat,
    ListAlignment, ListOffset, ListState, Render, Subscription, Task, Window, actions, img, list,
};
use project::Project;
use settings::Settings as _;
use ui::{IconButtonShape, Tooltip, prelude::*};
use util::{ResultExt as _, paths::PathExt as _};
use workspace::{
    ItemSettings, Pane, WorkspaceId,
    item::{Item, ItemBufferKind, ItemEvent, ProjectItem, TabContentParams},
};

pub use crate::document::{PageSize, WordDocument};

actions!(
    document_viewer,
    [
        /// Zoom in the document.
        ZoomIn,
        /// Zoom out the document.
        ZoomOut,
        /// Reset zoom to 100%.
        ResetZoom,
        /// Zoom so that the widest page fits the width of the view.
        FitWidth,
    ]
);

/// Pages are measured in points; at 100% zoom a point is shown as 96/72 pixels, which is the
/// size word processors use for 100%.
const PIXELS_PER_POINT: f32 = 96.0 / 72.0;
const MIN_ZOOM: f32 = 0.25;
const MAX_ZOOM: f32 = 4.0;
const ZOOM_STEP: f32 = 1.25;
/// Render scales are rounded to this step, so that small zoom changes reuse rendered pages.
const RENDER_SCALE_STEP: f32 = 0.25;
/// Rendering a page much larger than this costs a lot of memory for little visible gain.
const MAX_RENDER_SCALE: f32 = 6.0;
/// Rendered pages kept in memory. Pages scrolled out of view beyond this are rendered again.
const MAX_CACHED_PAGES: usize = 16;
const PAGE_GAP: f32 = 16.0;

pub fn init(cx: &mut App) {
    workspace::register_project_item::<DocumentView>(cx);
}

pub struct WordFormat;

impl FileFormat for WordFormat {
    type Content = WordDocument;
    const EXTENSIONS: &'static [&'static str] = &["docx", "doc"];
    const DISPLAY_NAME: &'static str = "Document";

    fn parse(bytes: &[u8]) -> Result<WordDocument> {
        WordDocument::parse(bytes)
    }
}

pub type DocumentItem = BinaryFileItem<WordFormat>;

struct RenderedPage {
    scale: f32,
    image: Arc<Image>,
}

pub enum DocumentViewEvent {
    TitleChanged,
}

/// A read-only tab that shows a Word document page by page, like a word processor does. Only the
/// pages near the visible area are rendered, on a background thread, and kept in a small cache.
pub struct DocumentView {
    item: Entity<DocumentItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    /// The document whose pages are in `list_state` and `rendered_pages`. Compared with the
    /// item's document to tell a reload apart from other item notifications.
    shown_document: Option<Arc<WordDocument>>,
    list_state: ListState,
    zoom: f32,
    rendered_pages: HashMap<usize, RenderedPage>,
    /// Most recently shown pages last, used to evict pages from `rendered_pages`.
    page_use_order: VecDeque<usize>,
    pending_renders: HashMap<usize, (f32, Task<()>)>,
    failed_pages: HashSet<usize>,
    current_page: usize,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DocumentViewEvent> for DocumentView {}

impl DocumentView {
    pub fn new(
        item: Entity<DocumentItem>,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let list_state = ListState::new(0, ListAlignment::Top, px(1000.));
        let this_handle = cx.weak_entity();
        list_state.set_scroll_handler(move |event, _, cx| {
            let first_visible_page = event.visible_range.start;
            this_handle
                .update(cx, |this, cx| {
                    if this.current_page != first_visible_page {
                        this.current_page = first_visible_page;
                        cx.notify();
                    }
                })
                .ok();
        });

        let subscriptions = vec![
            cx.subscribe(&item, |this, _, event, cx| match event {
                BinaryFileItemEvent::ContentChanged => this.sync_with_item(cx),
                BinaryFileItemEvent::FileChanged => cx.emit(DocumentViewEvent::TitleChanged),
            }),
            cx.on_release(|this, cx| this.clear_rendered_pages(cx)),
        ];

        let mut this = Self {
            item,
            project,
            focus_handle: cx.focus_handle(),
            shown_document: None,
            list_state,
            zoom: 1.0,
            rendered_pages: HashMap::default(),
            page_use_order: VecDeque::new(),
            pending_renders: HashMap::default(),
            failed_pages: HashSet::default(),
            current_page: 0,
            _subscriptions: subscriptions,
        };
        this.sync_with_item(cx);
        this
    }

    pub fn item(&self) -> &Entity<DocumentItem> {
        &self.item
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    pub fn page_count(&self) -> usize {
        self.shown_document
            .as_ref()
            .map_or(0, |document| document.pages().len())
    }

    pub fn rendered_page_count(&self) -> usize {
        self.rendered_pages.len()
    }

    fn sync_with_item(&mut self, cx: &mut Context<Self>) {
        let LoadState::Loaded(document) = self.item.read(cx).state().clone() else {
            cx.notify();
            return;
        };
        if self
            .shown_document
            .as_ref()
            .is_some_and(|shown| Arc::ptr_eq(shown, &document))
        {
            return;
        }

        self.clear_rendered_pages(cx);
        let scroll_top = self.list_state.logical_scroll_top();
        let page_count = document.pages().len();
        self.list_state.reset(page_count);
        if scroll_top.item_ix < page_count {
            self.list_state.scroll_to(scroll_top);
        }
        self.current_page = scroll_top.item_ix.min(page_count.saturating_sub(1));
        self.shown_document = Some(document);
        cx.notify();
    }

    fn clear_rendered_pages(&mut self, cx: &mut App) {
        for (_, page) in self.rendered_pages.drain() {
            page.image.remove_asset(cx);
        }
        self.page_use_order.clear();
        self.pending_renders.clear();
        self.failed_pages.clear();
    }

    fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if zoom == self.zoom {
            return;
        }
        // Keep the same spot of the same page at the top of the view.
        let scroll_top = self.list_state.logical_scroll_top();
        let ratio = zoom / self.zoom;
        self.zoom = zoom;
        self.list_state.remeasure();
        self.list_state.scroll_to(ListOffset {
            item_ix: scroll_top.item_ix,
            offset_in_item: scroll_top.offset_in_item * ratio,
        });
        cx.notify();
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom * ZOOM_STEP, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom / ZOOM_STEP, cx);
    }

    fn reset_zoom(&mut self, _: &ResetZoom, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(1.0, cx);
    }

    fn fit_width(&mut self, _: &FitWidth, _: &mut Window, cx: &mut Context<Self>) {
        let Some(document) = self.shown_document.as_ref() else {
            return;
        };
        let widest_page = document
            .pages()
            .iter()
            .map(|page| page.width)
            .fold(0.0, f32::max);
        let available_width =
            f32::from(self.list_state.viewport_bounds().size.width) - PAGE_GAP * 2.0;
        if widest_page > 0.0 && available_width > 0.0 {
            self.set_zoom(available_width / (widest_page * PIXELS_PER_POINT), cx);
        }
    }

    fn render_scale(&self, window: &Window) -> f32 {
        let scale = self.zoom * PIXELS_PER_POINT * window.scale_factor();
        ((scale / RENDER_SCALE_STEP).round() * RENDER_SCALE_STEP)
            .clamp(RENDER_SCALE_STEP, MAX_RENDER_SCALE)
    }

    fn mark_page_used(&mut self, page_index: usize, cx: &mut App) {
        self.page_use_order.retain(|&used| used != page_index);
        self.page_use_order.push_back(page_index);
        while self.page_use_order.len() > MAX_CACHED_PAGES {
            if let Some(evicted) = self.page_use_order.pop_front()
                && let Some(page) = self.rendered_pages.remove(&evicted)
            {
                page.image.remove_asset(cx);
            }
        }
    }

    fn request_page_render(&mut self, page_index: usize, scale: f32, cx: &mut Context<Self>) {
        let is_up_to_date = self
            .rendered_pages
            .get(&page_index)
            .is_some_and(|page| page.scale == scale);
        let is_pending = self
            .pending_renders
            .get(&page_index)
            .is_some_and(|(pending_scale, _)| *pending_scale == scale);
        if is_up_to_date || is_pending || self.failed_pages.contains(&page_index) {
            return;
        }
        let Some(document) = self.shown_document.clone() else {
            return;
        };

        let task = cx.spawn(async move |this, cx| {
            let png = cx
                .background_spawn(async move { document.render_page(page_index, scale) })
                .await;
            this.update(cx, |this, cx| {
                this.pending_renders.remove(&page_index);
                match png {
                    Ok(png) => {
                        let image = Arc::new(Image::from_bytes(ImageFormat::Png, png));
                        if let Some(replaced) = this
                            .rendered_pages
                            .insert(page_index, RenderedPage { scale, image })
                        {
                            replaced.image.remove_asset(cx);
                        }
                        this.mark_page_used(page_index, cx);
                    }
                    Err(error) => {
                        log::error!("{error:#}");
                        this.failed_pages.insert(page_index);
                    }
                }
                cx.notify();
            })
            .log_err();
        });
        self.pending_renders.insert(page_index, (scale, task));
    }

    fn render_page(
        &mut self,
        page_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(page) = self
            .shown_document
            .as_ref()
            .and_then(|document| document.pages().get(page_index).copied())
        else {
            return div().into_any_element();
        };

        let scale = self.render_scale(window);
        self.request_page_render(page_index, scale, cx);
        if self.rendered_pages.contains_key(&page_index) {
            self.mark_page_used(page_index, cx);
        }

        // While a page renders at a new zoom level, the previous render is stretched to fit, so
        // zooming does not flash blank pages.
        let content = if let Some(rendered) = self.rendered_pages.get(&page_index) {
            img(rendered.image.clone()).size_full().into_any_element()
        } else if self.failed_pages.contains(&page_index) {
            h_flex()
                .size_full()
                .justify_center()
                .child(
                    Label::new(format!("Could not render page {}", page_index + 1))
                        .color(Color::Error),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };

        let page_width = page.width * PIXELS_PER_POINT * self.zoom;
        let page_height = page.height * PIXELS_PER_POINT * self.zoom;
        h_flex()
            .w_full()
            .justify_center()
            .py(px(PAGE_GAP / 2.0))
            .child(
                div()
                    .flex_none()
                    .w(px(page_width))
                    .h(px(page_height))
                    .bg(gpui::white())
                    .shadow_md()
                    .child(content),
            )
            .into_any_element()
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let page_count = self.page_count();
        let warnings = self
            .shown_document
            .as_ref()
            .map(|document| document.warnings().to_vec())
            .unwrap_or_default();

        h_flex()
            .w_full()
            .flex_none()
            .gap_2()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .when(page_count > 0, |bar| {
                bar.child(
                    Label::new(format!(
                        "Page {} of {page_count}",
                        (self.current_page + 1).min(page_count)
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .when(!warnings.is_empty(), |bar| {
                let details: SharedString = warnings
                    .iter()
                    .take(10)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into();
                bar.child(
                    h_flex()
                        .id("document-viewer-warnings")
                        .gap_1()
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Small)
                                .color(Color::Warning),
                        )
                        .child(
                            Label::new("Some content is shown approximately")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .tooltip(Tooltip::text(details)),
                )
            })
            .child(div().flex_1())
            .child(
                IconButton::new("document-viewer-zoom-out", IconName::Dash)
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Zoom Out"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.zoom_out(&ZoomOut, window, cx);
                    })),
            )
            .child(
                Label::new(format!("{:.0}%", self.zoom * 100.0))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                IconButton::new("document-viewer-zoom-in", IconName::Plus)
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Zoom In"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.zoom_in(&ZoomIn, window, cx);
                    })),
            )
            .child(
                Button::new("document-viewer-fit-width", "Fit Width")
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.fit_width(&FitWidth, window, cx);
                    })),
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

impl Render for DocumentView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.item.read(cx).state() {
            LoadState::Failed(error) => render_message(error.clone(), Color::Error),
            LoadState::Loading if self.shown_document.is_none() => {
                render_message("Loading…".into(), Color::Muted)
            }
            LoadState::Loading | LoadState::Loaded(_) if self.page_count() == 0 => {
                render_message("This document has no pages".into(), Color::Muted)
            }
            LoadState::Loading | LoadState::Loaded(_) => list(
                self.list_state.clone(),
                cx.processor(|this, page_index, window, cx| {
                    this.render_page(page_index, window, cx)
                }),
            )
            .size_full()
            .into_any_element(),
        };

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .key_context("DocumentViewer")
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::reset_zoom))
            .on_action(cx.listener(Self::fit_width))
            .bg(cx.theme().colors().panel_background)
            .child(div().flex_1().min_h_0().w_full().child(body))
            .child(self.render_status_bar(cx))
    }
}

impl Focusable for DocumentView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for DocumentView {
    type Event = DocumentViewEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            DocumentViewEvent::TitleChanged => f(ItemEvent::UpdateTab),
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
            cx.new(|cx| DocumentView::new(item, project, window, cx)),
        ))
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.item.read(cx).is_deleted()
    }

    fn buffer_kind(&self, _: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }
}

impl ProjectItem for DocumentView {
    type Item = DocumentItem;

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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use util::path;
    use workspace::{AppState, OpenOptions, Workspace};

    const SAMPLE_DOCX: &[u8] = include_bytes!("../test_data/sample.docx");
    const SAMPLE_DOC: &[u8] = include_bytes!("../test_data/sample.doc");

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

    async fn open_document(
        abs_path: &str,
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Result<Entity<DocumentView>> {
        open_path(abs_path, workspace, cx)
            .await?
            .downcast::<DocumentView>()
            .context("the file did not open as a document")
    }

    async fn test_workspace(
        fs: Arc<FakeFs>,
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx))
    }

    #[gpui::test]
    async fn test_opens_word_documents_as_pages(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "notes.txt": "plain" }))
            .await;
        fs.insert_file(path!("/project/report.docx"), SAMPLE_DOCX.to_vec())
            .await;
        fs.insert_file(path!("/project/legacy.doc"), SAMPLE_DOC.to_vec())
            .await;
        let (workspace, cx) = test_workspace(fs, cx).await;

        let result: Result<()> = async {
            for (abs_path, file_name) in [
                (path!("/project/report.docx"), "report.docx"),
                (path!("/project/legacy.doc"), "legacy.doc"),
            ] {
                let view = open_document(abs_path, &workspace, cx).await?;
                cx.run_until_parked();
                view.read_with(cx, |view, cx| {
                    assert!(matches!(view.item().read(cx).state(), LoadState::Loaded(_)));
                    assert_eq!(view.page_count(), 2, "{file_name}");
                    assert_eq!(view.tab_content_text(0, cx), file_name);
                });
            }

            let text_item = open_path(path!("/project/notes.txt"), &workspace, cx).await?;
            assert!(text_item.downcast::<DocumentView>().is_none());
            Ok(())
        }
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[gpui::test]
    async fn test_renders_visible_pages(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({})).await;
        fs.insert_file(path!("/project/report.docx"), SAMPLE_DOCX.to_vec())
            .await;
        let (workspace, cx) = test_workspace(fs, cx).await;

        let result: Result<()> = async {
            let view = open_document(path!("/project/report.docx"), &workspace, cx).await?;
            cx.run_until_parked();
            // Rendering finishes on a background thread and then redraws the view.
            for _ in 0..3 {
                cx.update(|window, _| window.refresh());
                cx.run_until_parked();
            }
            view.read_with(cx, |view, _| {
                assert!(view.rendered_page_count() >= 1);
            });
            Ok(())
        }
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[gpui::test]
    async fn test_zoom_actions(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({})).await;
        fs.insert_file(path!("/project/report.docx"), SAMPLE_DOCX.to_vec())
            .await;
        let (workspace, cx) = test_workspace(fs, cx).await;

        let result: Result<()> = async {
            let view = open_document(path!("/project/report.docx"), &workspace, cx).await?;
            cx.run_until_parked();

            let dispatch = |action: &dyn gpui::Action, cx: &mut VisualTestContext| {
                cx.update(|window, cx| {
                    let focus_handle = view.read(cx).focus_handle(cx);
                    focus_handle.focus(window, cx);
                    window.dispatch_action(action.boxed_clone(), cx);
                });
            };

            dispatch(&ZoomIn, cx);
            assert_eq!(view.read_with(cx, |view, _| view.zoom()), ZOOM_STEP);
            dispatch(&ResetZoom, cx);
            assert_eq!(view.read_with(cx, |view, _| view.zoom()), 1.0);
            for _ in 0..20 {
                dispatch(&ZoomOut, cx);
            }
            assert_eq!(view.read_with(cx, |view, _| view.zoom()), MIN_ZOOM);
            for _ in 0..20 {
                dispatch(&ZoomIn, cx);
            }
            assert_eq!(view.read_with(cx, |view, _| view.zoom()), MAX_ZOOM);
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
        fs.insert_file(path!("/project/report.docx"), SAMPLE_DOCX.to_vec())
            .await;
        let (workspace, cx) = test_workspace(fs.clone(), cx).await;

        let result: Result<()> = async {
            let view = open_document(path!("/project/report.docx"), &workspace, cx).await?;
            cx.run_until_parked();
            assert_eq!(view.read_with(cx, |view, _| view.page_count()), 2);

            fs.insert_file(path!("/project/report.docx"), b"broken".to_vec())
                .await;
            cx.run_until_parked();
            view.read_with(cx, |view, cx| {
                assert!(matches!(view.item().read(cx).state(), LoadState::Failed(_)));
            });

            fs.insert_file(path!("/project/report.docx"), SAMPLE_DOCX.to_vec())
                .await;
            cx.run_until_parked();
            view.read_with(cx, |view, cx| {
                assert!(matches!(view.item().read(cx).state(), LoadState::Loaded(_)));
                assert_eq!(view.page_count(), 2);
            });
            Ok(())
        }
        .await;
        assert!(result.is_ok(), "{result:?}");
    }
}
