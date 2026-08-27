//! The window: a connection tree on the left, a video wall on the right.

use crate::model::{
    Connection, GridView, LayoutMode, Library, Node, ThemePref, Transport,
};
use crate::stream::{Player, Status};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ObjectFit,
    ParentElement as _, RenderImage, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, Task, Window, div, img, px, relative, svg,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::list::ListItem;
use gpui_component::theme::{Theme, ThemeMode};
use gpui_component::tree::{TreeItem, TreeState, tree};
use gpui_component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// A tree row being dragged. Carries the label so the preview can show it
/// without reaching back into the library.
#[derive(Clone)]
struct DraggedNode {
    id: Uuid,
    label: SharedString,
}

/// An open tile being dragged to a new position on the wall.
#[derive(Clone)]
struct DraggedTile {
    id: Uuid,
    label: SharedString,
}

/// The chip that follows the cursor during a drag.
struct DragPreview {
    label: SharedString,
}

impl gpui::Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(px(4.0))
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .text_xs()
            .child(self.label.clone())
    }
}

/// Roughly 30 fps. The decoders push frames into a mailbox; this is just how
/// often we look.
const REFRESH_INTERVAL: Duration = Duration::from_millis(33);

pub struct PlayerApp {
    library: Library,
    tree: Entity<TreeState>,
    selected: Option<Uuid>,
    open: Vec<OpenStream>,
    form: Option<Form>,
    /// The tile under the cursor. Its header is the only one drawn.
    hovered_tile: Option<Uuid>,
    /// A working copy of the active view. A wall is locked until it is
    /// unlocked for editing, and edits live here until they are saved, so
    /// nothing touches the config file until you say so.
    draft: Option<GridView>,
    error: Option<SharedString>,
    _refresh: Task<()>,
}

struct OpenStream {
    id: Uuid,
    player: Player,
    texture: Option<Arc<RenderImage>>,
}

/// The add/edit sheet. Held here rather than in its own entity so saving is a
/// plain method call instead of an event round-trip.
/// What the add/edit sheet is editing. Groups and walls reuse it but show only
/// the name field.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FormTarget {
    Connection,
    Group,
    View,
}

struct Form {
    editing: Option<Uuid>,
    parent: Option<Uuid>,
    target: FormTarget,
    name: Entity<InputState>,
    url: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    transport: Transport,
}

impl PlayerApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let library = Library::load();
        let tree = cx.new(|cx| TreeState::new(cx).items(tree_items(&library.roots)));

        let restore: Vec<Connection> = library
            .open
            .iter()
            .filter_map(|id| library.connection(*id).cloned())
            .collect();

        let refresh = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                let still_alive = this
                    .update(cx, |this, cx| {
                        if !this.open.is_empty() {
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !still_alive {
                    break;
                }
            }
        });

        apply_theme(library.theme, window, cx);
        // Only matters while the preference is System, but the subscription is
        // cheap enough to hold unconditionally.
        cx.observe_window_appearance(window, |this, window, cx| {
            if this.library.theme == ThemePref::System {
                Theme::sync_system_appearance(Some(window), cx);
            }
        })
        .detach();

        let mut this = Self {
            library,
            tree,
            selected: None,
            open: Vec::new(),
            form: None,
            hovered_tile: None,
            draft: None,
            error: None,
            _refresh: refresh,
        };
        for connection in restore {
            this.push_stream(connection);
        }
        this
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.library.sidebar_hidden = !self.library.sidebar_hidden;
        self.save_library();
        cx.notify();
    }

    fn cycle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.library.theme = self.library.theme.next();
        apply_theme(self.library.theme, window, cx);
        self.save_library();
        cx.notify();
    }

    fn rebuild_tree(&mut self, cx: &mut Context<Self>) {
        let items = tree_items(&self.library.roots);
        self.tree.update(cx, |state, cx| state.set_items(items, cx));
        cx.notify();
    }

    fn save_library(&mut self) {
        self.library.open = self.open.iter().map(|s| s.id).collect();
        if let Err(e) = self.library.save() {
            self.error = Some(format!("Could not save library: {e}").into());
        }
    }

    /// The group to add new items into: the selection if it is a group, the
    /// selected connection's parent otherwise.
    fn target_group(&self) -> Option<Uuid> {
        let selected = self.selected?;
        match self.library.find(selected)? {
            Node::Group { id, .. } => Some(*id),
            Node::Stream(_) => parent_of(&self.library.roots, selected),
        }
    }

    fn on_item_click(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id);
        match self.library.find(id) {
            Some(Node::Group { id, expanded, .. }) => {
                // The tree widget has already flipped its own copy; keep ours in
                // step so the next rebuild starts from the same shape.
                let (id, expanded) = (*id, !*expanded);
                self.library.set_expanded(id, expanded);
                self.save_library();
            }
            Some(Node::Stream(connection)) => {
                let connection = connection.clone();
                self.open_stream(connection, window);
            }
            None => {}
        }
        cx.notify();
    }

    /// Dropping onto a group moves into it; dropping onto a connection moves
    /// alongside that connection, the way Zed's project panel behaves.
    fn drop_onto(&mut self, dragged: Uuid, target: Uuid, cx: &mut Context<Self>) {
        let new_parent = match self.library.find(target) {
            Some(Node::Group { id, .. }) => Some(*id),
            Some(Node::Stream(_)) => self.library.parent_of(target),
            None => return,
        };
        self.move_node(dragged, new_parent, cx);
    }

    fn move_node(&mut self, dragged: Uuid, new_parent: Option<Uuid>, cx: &mut Context<Self>) {
        if self.library.move_node(dragged, new_parent) {
            self.save_library();
            self.rebuild_tree(cx);
        }
    }

    /// Hovering a collapsed group mid-drag opens it, so you can drop deeper
    /// without letting go first.
    fn expand_for_drag(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let collapsed = matches!(
            self.library.find(id),
            Some(Node::Group { expanded: false, .. })
        );
        if collapsed {
            self.library.set_expanded(id, true);
            self.rebuild_tree(cx);
        }
    }

    /// In `Single` the wall shows one stream at a time, so opening replaces
    /// what is there; in `Multi` it adds a tile.
    fn open_stream(&mut self, connection: Connection, window: &mut Window) {
        // A saved view pins specific positions; opening freely leaves it.
        self.library.active_view = None;
        if self.library.layout == LayoutMode::Single {
            self.close_all(window);
        } else if self.open.iter().any(|s| s.id == connection.id) {
            return;
        }
        self.push_stream(connection);
        self.save_library();
    }

    fn push_stream(&mut self, connection: Connection) {
        if self.open.iter().any(|s| s.id == connection.id) {
            return;
        }
        self.open.push(OpenStream {
            id: connection.id,
            player: Player::start(connection),
            texture: None,
        });
    }

    /// Opens every connection in a group as a set, switching to the grid so
    /// they are all visible.
    fn open_group(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let connections = self.library.connections_under(id);
        if connections.is_empty() {
            return;
        }
        self.library.active_view = None;
        self.close_all(window);
        if connections.len() > 1 {
            self.library.layout = LayoutMode::Multi;
        }
        for connection in connections {
            self.push_stream(connection);
        }
        self.save_library();
        cx.notify();
    }

    fn close_all(&mut self, window: &mut Window) {
        for closed in self.open.drain(..) {
            if let Some(texture) = closed.texture {
                let _ = window.drop_image(texture);
            }
        }
    }

    fn toggle_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.library.layout = self.library.layout.toggled();
        // Collapsing to one view keeps the stream that was opened last.
        if self.library.layout == LayoutMode::Single && self.open.len() > 1 {
            let keep = self.open.pop();
            self.close_all(window);
            self.open.extend(keep);
        }
        self.save_library();
        cx.notify();
    }

    /// Loads a saved wall: opens exactly its connections and shows them at the
    /// positions the view records.
    fn load_view(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.library.view(id) else {
            return;
        };
        let wanted: Vec<Uuid> = view.cells.iter().map(|c| c.connection).collect();
        let connections: Vec<Connection> = wanted
            .iter()
            .filter_map(|c| self.library.connection(*c).cloned())
            .collect();

        self.draft = None;
        self.close_all(window);
        for connection in connections {
            self.push_stream(connection);
        }
        self.library.active_view = Some(id);
        self.save_library();
        cx.notify();
    }

    fn close_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.draft = None;
        self.library.active_view = None;
        self.close_all(window);
        self.save_library();
        cx.notify();
    }

    /// Snapshots what is on the wall into a new saved view, filling the grid
    /// left to right.
    fn save_wall_as_view(&mut self, cx: &mut Context<Self>) {
        if self.open.is_empty() {
            self.error = Some("Open some streams first.".into());
            cx.notify();
            return;
        }
        let cols = grid_columns(self.open.len());
        let rows = self.open.len().div_ceil(cols);
        let mut view = GridView::new(format!("View {}", self.library.views.len() + 1), rows, cols);
        for (ix, stream) in self.open.iter().enumerate() {
            view.place(stream.id, ix / cols, ix % cols);
        }
        let id = view.id;
        self.library.views.push(view);
        self.library.active_view = Some(id);
        self.error = None;
        self.save_library();
        cx.notify();
    }

    /// The grid on screen: the draft while editing, otherwise the saved view.
    fn active_grid(&self) -> Option<&GridView> {
        self.draft.as_ref().or_else(|| {
            self.library
                .active_view
                .and_then(|id| self.library.view(id))
        })
    }

    fn editing(&self) -> bool {
        self.draft.is_some()
    }

    /// Unlocks the active wall for editing by taking a working copy.
    fn begin_edit(&mut self, cx: &mut Context<Self>) {
        if self.draft.is_some() {
            return;
        }
        if let Some(view) = self
            .library
            .active_view
            .and_then(|id| self.library.view(id))
            .cloned()
        {
            self.draft = Some(view);
            cx.notify();
        }
    }

    /// Writes the working copy back and locks the wall again.
    fn commit_edit(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.take() else {
            return;
        };
        if let Some(view) = self.library.view_mut(draft.id) {
            *view = draft;
        }
        self.save_library();
        cx.notify();
    }

    /// Throws the working copy away and replays the saved wall.
    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.draft.take().is_none() {
            return;
        }
        if let Some(id) = self.library.active_view {
            self.load_view(id, window, cx);
        }
        cx.notify();
    }

    fn resize_view(&mut self, rows: isize, cols: isize, cx: &mut Context<Self>) {
        let Some(view) = self.draft.as_mut() else {
            return;
        };
        view.rows = view.rows.saturating_add_signed(rows).max(1);
        view.cols = view.cols.saturating_add_signed(cols).max(1);
        view.clamp();
        cx.notify();
    }

    /// Puts a connection at a grid position, starting its stream if needed.
    fn place_in_view(
        &mut self,
        connection: Uuid,
        row: usize,
        col: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.draft.as_mut() else {
            return;
        };
        // Whatever sat here is evicted; close its stream so the wall matches.
        let evicted: Vec<Uuid> = view
            .cells
            .iter()
            .filter(|c| c.covers(row, col) && c.connection != connection)
            .map(|c| c.connection)
            .collect();
        view.place(connection, row, col);

        for id in evicted {
            if let Some(ix) = self.open.iter().position(|s| s.id == id) {
                let closed = self.open.remove(ix);
                if let Some(texture) = closed.texture {
                    let _ = window.drop_image(texture);
                }
            }
        }
        if let Some(connection) = self.library.connection(connection).cloned() {
            self.push_stream(connection);
        }
        cx.notify();
    }

    fn delete_view(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.library.views.retain(|v| v.id != id);
        if self.library.active_view == Some(id) {
            self.library.active_view = None;
            self.close_all(window);
        }
        self.save_library();
        cx.notify();
    }

    /// Moves an open tile in front of another, so the grid can be arranged.
    fn reorder_tile(&mut self, dragged: Uuid, target: Uuid, cx: &mut Context<Self>) {
        if dragged == target {
            return;
        }
        let Some(from) = self.open.iter().position(|s| s.id == dragged) else {
            return;
        };
        let Some(to) = self.open.iter().position(|s| s.id == target) else {
            return;
        };
        let moved = self.open.remove(from);
        self.open.insert(to, moved);
        self.save_library();
        cx.notify();
    }

    fn close_stream(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.open.iter().position(|s| s.id == id) {
            let closed = self.open.remove(ix);
            if let Some(texture) = closed.texture {
                let _ = window.drop_image(texture);
            }
            if let Some(view) = self.draft.as_mut() {
                view.remove(id);
            }
            self.save_library();
        }
        cx.notify();
    }

    fn add_group(&mut self, cx: &mut Context<Self>) {
        let parent = self.target_group();
        let group = Node::group("New group");
        let id = group.id();
        self.library.insert(parent, group);
        self.save_library();
        self.selected = Some(id);
        self.rebuild_tree(cx);
    }

    fn open_form(&mut self, editing: Option<Connection>, window: &mut Window, cx: &mut Context<Self>) {
        let existing = editing.clone();
        let name = existing.as_ref().map(|c| c.name.clone()).unwrap_or_default();
        let url = existing
            .as_ref()
            .map(|c| c.url.clone())
            .unwrap_or_else(|| "rtsp://".to_string());
        let username = existing
            .as_ref()
            .and_then(|c| c.username.clone())
            .unwrap_or_default();
        let password = existing
            .as_ref()
            .and_then(|c| c.password.clone())
            .unwrap_or_default();

        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Front door")
                .default_value(name)
        });
        let url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("rtsp://192.168.1.50:554/stream1")
                .default_value(url)
        });
        let username_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("optional")
                .default_value(username)
        });
        let password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("optional")
                .masked(true)
                .default_value(password)
        });

        name_input.update(cx, |state, cx| state.focus(window, cx));

        self.form = Some(Form {
            editing: existing.as_ref().map(|c| c.id),
            parent: self.target_group(),
            target: FormTarget::Connection,
            name: name_input,
            url: url_input,
            username: username_input,
            password: password_input,
            transport: existing.map(|c| c.transport).unwrap_or_default(),
        });
        cx.notify();
    }

    fn open_group_form(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.library.find(id) else {
            return;
        };
        let current = node.name().to_string();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Group name")
                .default_value(current)
        });
        name.update(cx, |state, cx| state.focus(window, cx));

        // The other fields are never rendered for a group, but the struct needs
        // them; empty states are cheap.
        let blank = |window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx))
        };
        self.form = Some(Form {
            editing: Some(id),
            parent: None,
            target: FormTarget::Group,
            name,
            url: blank(window, cx),
            username: blank(window, cx),
            password: blank(window, cx),
            transport: Transport::default(),
        });
        cx.notify();
    }

    /// Rename sheet for a saved wall.
    fn open_view_form(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.library.view(id) else {
            return;
        };
        let current = view.name.clone();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Wall name")
                .default_value(current)
        });
        name.update(cx, |state, cx| state.focus(window, cx));

        let blank = |window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx))
        };
        self.form = Some(Form {
            editing: Some(id),
            parent: None,
            target: FormTarget::View,
            name,
            url: blank(window, cx),
            username: blank(window, cx),
            password: blank(window, cx),
            transport: Transport::default(),
        });
        cx.notify();
    }

    fn submit_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.form.take() else { return };

        let name = form.name.read(cx).value().trim().to_string();

        if form.target == FormTarget::Group {
            if let Some(id) = form.editing
                && !name.is_empty()
            {
                self.library.rename_group(id, name);
                self.save_library();
            }
            self.error = None;
            self.rebuild_tree(cx);
            return;
        }

        if form.target == FormTarget::View {
            if let Some(id) = form.editing
                && !name.is_empty()
            {
                if let Some(view) = self.library.view_mut(id) {
                    view.name = name.clone();
                }
                // The working copy carries the name until the wall is saved.
                if let Some(draft) = self.draft.as_mut() {
                    draft.name = name;
                }
                self.save_library();
            }
            self.error = None;
            cx.notify();
            return;
        }

        let url = form.url.read(cx).value().trim().to_string();
        let username = form.username.read(cx).value().trim().to_string();
        let password = form.password.read(cx).unmask_value().to_string();

        if url.is_empty() {
            self.error = Some("A stream needs a URL.".into());
            self.form = Some(form);
            cx.notify();
            return;
        }

        let mut connection = match form.editing.and_then(|id| self.library.connection(id)) {
            Some(existing) => existing.clone(),
            None => Connection::new(String::new(), String::new()),
        };
        connection.name = if name.is_empty() {
            url.clone()
        } else {
            name
        };
        connection.url = url;
        connection.username = (!username.is_empty()).then_some(username);
        connection.password = (!password.is_empty()).then_some(password);
        connection.transport = form.transport;

        match form.editing {
            Some(_) => self.library.update_connection(connection),
            None => {
                let id = connection.id;
                self.library.insert(form.parent, Node::Stream(connection));
                self.selected = Some(id);
            }
        }
        self.error = None;
        self.save_library();
        self.rebuild_tree(cx);
    }

    fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.take() else {
            return;
        };
        // Close anything inside the removed subtree before it disappears.
        let closing: Vec<Uuid> = self
            .open
            .iter()
            .map(|s| s.id)
            .filter(|open_id| self.library.is_ancestor(id, *open_id))
            .collect();
        for open_id in closing {
            self.close_stream(open_id, window, cx);
        }
        self.library.remove(id);
        self.save_library();
        self.rebuild_tree(cx);
    }

    /// Pull any newly decoded frames onto the GPU. Called once per render so a
    /// tile never shows a frame older than one refresh tick.
    fn sync_textures(&mut self, window: &mut Window) {
        for stream in &mut self.open {
            let Some(frame) = stream.player.take_frame() else {
                continue;
            };
            let Some(buffer) =
                image::ImageBuffer::from_raw(frame.width, frame.height, frame.bgra)
            else {
                continue;
            };
            let texture = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
            if let Some(previous) = stream.texture.replace(texture) {
                // Without this the atlas keeps every frame we ever uploaded.
                let _ = window.drop_image(previous);
            }
        }
    }
}

/// Right-click menu for a tree row. Groups can take new children; connections
/// can be opened and edited.
fn row_menu(
    menu: PopupMenu,
    id: Uuid,
    is_group: bool,
    this: &Entity<PlayerApp>,
) -> PopupMenu {
    let menu = if is_group {
        menu.item(PopupMenuItem::new("Open all").on_click({
            let this = this.clone();
            move |_, window, app| {
                this.update(app, |this, cx| this.open_group(id, window, cx));
            }
        }))
        .item(PopupMenuItem::separator())
        .item(PopupMenuItem::new("New stream\u{2026}").on_click({
            let this = this.clone();
            move |_, window, app| {
                this.update(app, |this, cx| {
                    this.selected = Some(id);
                    this.open_form(None, window, cx);
                });
            }
        }))
        .item(PopupMenuItem::new("New group").on_click({
            let this = this.clone();
            move |_, _, app| {
                this.update(app, |this, cx| {
                    this.selected = Some(id);
                    this.add_group(cx);
                });
            }
        }))
        .item(PopupMenuItem::separator())
        .item(PopupMenuItem::new("Rename\u{2026}").on_click({
            let this = this.clone();
            move |_, window, app| {
                this.update(app, |this, cx| this.open_group_form(id, window, cx));
            }
        }))
    } else {
        menu.item(PopupMenuItem::new("Open").on_click({
            let this = this.clone();
            move |_, window, app| {
                this.update(app, |this, cx| {
                    if let Some(connection) = this.library.connection(id).cloned() {
                        this.open_stream(connection, window);
                        cx.notify();
                    }
                });
            }
        }))
        .item(PopupMenuItem::new("Edit\u{2026}").on_click({
            let this = this.clone();
            move |_, window, app| {
                this.update(app, |this, cx| {
                    if let Some(connection) = this.library.connection(id).cloned() {
                        this.open_form(Some(connection), window, cx);
                    }
                });
            }
        }))
    };

    menu.item(PopupMenuItem::separator())
        .item(PopupMenuItem::new("Delete").on_click({
            let this = this.clone();
            move |_, window, app| {
                this.update(app, |this, cx| {
                    this.selected = Some(id);
                    this.delete_selected(window, cx);
                });
            }
        }))
}

/// A square icon button, used for the chrome controls.
fn icon_button(
    id: &'static str,
    path: &'static str,
    tooltip: &'static str,
    cx: &mut Context<PlayerApp>,
    on_click: impl Fn(&mut PlayerApp, &mut Window, &mut Context<PlayerApp>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(22.0))
        .flex_none()
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().secondary))
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(tooltip).build(window, cx)
        })
        .child(
            svg()
                .path(path)
                .size(px(14.0))
                .flex_none()
                .text_color(cx.theme().muted_foreground),
        )
        .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
}

fn apply_theme(pref: ThemePref, window: &mut Window, cx: &mut Context<PlayerApp>) {
    match pref {
        ThemePref::System => Theme::sync_system_appearance(Some(window), cx),
        ThemePref::Light => Theme::change(ThemeMode::Light, Some(window), cx),
        ThemePref::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
    }
}

fn parent_of(nodes: &[Node], child: Uuid) -> Option<Uuid> {
    for node in nodes {
        let Node::Group { id, children, .. } = node else {
            continue;
        };
        if children.iter().any(|c| c.id() == child) {
            return Some(*id);
        }
        if let Some(found) = parent_of(children, child) {
            return Some(found);
        }
    }
    None
}

fn tree_items(nodes: &[Node]) -> Vec<TreeItem> {
    nodes
        .iter()
        .map(|node| match node {
            Node::Group {
                id,
                name,
                children,
                expanded,
            } => TreeItem::new(format!("g:{id}"), name.clone())
                .children(tree_items(children))
                .expanded(*expanded),
            Node::Stream(connection) => {
                TreeItem::new(format!("s:{}", connection.id), connection.name.clone())
            }
        })
        .collect()
}

/// Tiles are laid out in the squarest grid that fits, so four streams give 2x2
/// rather than a 1x4 strip.
fn grid_columns(count: usize) -> usize {
    (count as f32).sqrt().ceil() as usize
}

impl gpui::Render for PlayerApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_textures(window);

        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .when(!self.library.sidebar_hidden, |root| {
                root.child(self.render_sidebar(cx))
            })
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.render_chrome(cx))
                    .child(self.render_wall(cx)),
            )
            .when_some(self.form.as_ref(), |this, _| this.child(self.render_form(cx)))
    }
}

impl PlayerApp {
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(272.0))
            .flex_none()
            .h_full()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().sidebar_border)
                    .child(
                        Button::new("add-stream")
                            .label("Add stream")
                            .primary()
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_form(None, window, cx)
                            })),
                    )
                    .child(
                        Button::new("add-group")
                            .label("Group")
                            .outline()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.add_group(cx))),
                    )
                    .child(
                        Button::new("layout")
                            .label(self.library.layout.label())
                            .ghost()
                            .small()
                            .tooltip(match self.library.layout {
                                LayoutMode::Single => "One at a time \u{2014} opening replaces",
                                LayoutMode::Multi => "Many \u{2014} opening adds a tile",
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_layout(window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .id("tree-pane")
                    .flex_1()
                    .min_h_0()
                    // Empty space below the rows is the top level, so a node can
                    // be dragged out of every group.
                    .on_drop::<DraggedNode>({
                        let this = cx.entity();
                        move |dragged, _, app| {
                            let dragged = dragged.id;
                            this.update(app, |this, cx| this.move_node(dragged, None, cx));
                        }
                    })
                    .child(tree(&self.tree, {
                        // The render closure only gets `&mut App`, so route
                        // clicks and drops back through a handle to this view.
                        let this = cx.entity();
                        let muted = cx.theme().muted_foreground;
                        move |ix, entry, _selected, _window, _cx| {
                            // `entry.is_folder()` only means "has children", so
                            // an empty group would render and behave as a
                            // stream. The kind is encoded in the id instead.
                            let (kind, raw_id) = entry
                                .item()
                                .id
                                .split_once(':')
                                .unwrap_or(("s", entry.item().id.as_ref()));
                            let is_group = kind == "g";
                            let id = raw_id.parse::<Uuid>().ok();
                            let collapsed = is_group && !entry.is_expanded();
                            let chevron = if entry.is_expanded() {
                                "icons/chevron-down.svg"
                            } else {
                                "icons/chevron-right.svg"
                            };
                            let glyph = match (is_group, entry.is_expanded()) {
                                (true, true) => "icons/folder-open.svg",
                                (true, false) => "icons/folder.svg",
                                (false, _) => "icons/camera.svg",
                            };
                            let label = entry.item().label.clone();
                            let this = this.clone();
                            ListItem::new(ix)
                                .pl(px(8.0 + 14.0 * entry.depth() as f32))
                                .child(
                                    div()
                                        .id(("row", ix))
                                        .w_full()
                                        .flex()
                                        .gap_2()
                                        .items_center()
                                        // Leaves keep the chevron's width so their
                                        // icons line up under the folder's.
                                        .child(if is_group {
                                            svg()
                                                .path(chevron)
                                                .size(px(12.0))
                                                .flex_none()
                                                .text_color(muted)
                                                .into_any_element()
                                        } else {
                                            div().w(px(12.0)).flex_none().into_any_element()
                                        })
                                        .child(
                                            svg()
                                                .path(glyph)
                                                .size(px(14.0))
                                                .flex_none()
                                                .text_color(muted),
                                        )
                                        .child(label.clone())
                                        .when_some(id, |row, node_id| {
                                            row.on_drag(
                                                DraggedNode {
                                                    id: node_id,
                                                    label: label.clone(),
                                                },
                                                |dragged, _, _, cx| {
                                                    let label = dragged.label.clone();
                                                    cx.new(|_| DragPreview { label })
                                                },
                                            )
                                            .drag_over::<DraggedNode>(|mut style, _, _, cx| {
                                                style.background =
                                                    Some(cx.theme().drop_target.into());
                                                style
                                            })
                                            .on_drop::<DraggedNode>({
                                                let this = this.clone();
                                                move |dragged, _, app| {
                                                    let dragged = dragged.id;
                                                    this.update(app, |this, cx| {
                                                        this.drop_onto(dragged, node_id, cx)
                                                    });
                                                }
                                            })
                                            .when(collapsed, |row| {
                                                let this = this.clone();
                                                row.on_drag_move::<DraggedNode>(
                                                    move |_, _, app| {
                                                        this.update(app, |this, cx| {
                                                            this.expand_for_drag(node_id, cx)
                                                        });
                                                    },
                                                )
                                            })
                                        })
                                        .context_menu({
                                            let this = this.clone();
                                            move |menu, _, _| match id {
                                                Some(id) => row_menu(menu, id, is_group, &this),
                                                None => menu,
                                            }
                                        }),
                                )
                                .on_click(move |_, window, app| {
                                    if let Some(id) = id {
                                        this.update(app, |this, cx| {
                                            this.on_item_click(id, window, cx)
                                        });
                                    }
                                })
                        }
                    })),
            )
            .when(self.library.roots.is_empty(), |this| {
                this.child(
                    div()
                        .p_3()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("No connections yet. Add one to get started."),
                )
            })
            .child(self.render_views(cx))
            .child(
                v_flex()
                    .p_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(cx.theme().sidebar_border)
                    .child(
                        Button::new("theme")
                            .label(self.library.theme.label())
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cycle_theme(window, cx)
                            })),
                    )
                    .when_some(self.error.clone(), |this, error| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(error),
                        )
                    }),
            )
    }

    /// Saved walls, listed under the tree.
    fn render_views(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.library.active_view;

        v_flex()
            .flex_none()
            .max_h(px(180.0))
            .border_t_1()
            .border_color(cx.theme().sidebar_border)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Views"),
                    )
                    .child(
                        Button::new("save-view")
                            .label("Save wall")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, _, cx| this.save_wall_as_view(cx))),
                    ),
            )
            .child(
                v_flex()
                    .id("views-list")
                    .px_1()
                    .pb_1()
                    .gap_px()
                    .overflow_y_scroll()
                    .children(self.library.views.iter().map(|view| {
                        let id = view.id;
                        let selected = active == Some(id);
                        h_flex()
                            .id(("view", id.as_u128() as usize))
                            .px_2()
                            .py_1()
                            .gap_2()
                            .rounded(px(4.0))
                            .justify_between()
                            .when(selected, |el| el.bg(cx.theme().list_active))
                            .hover(|el| el.bg(cx.theme().list_hover))
                            .child(div().text_sm().truncate().child(view.name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("{}\u{00d7}{}", view.rows, view.cols)),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.load_view(id, window, cx)
                            }))
                            .context_menu({
                                let this = cx.entity();
                                move |menu, _, _| {
                                    menu.item(PopupMenuItem::new("Rename\u{2026}").on_click({
                                        let this = this.clone();
                                        move |_, window, app| {
                                            this.update(app, |this, cx| {
                                                this.open_view_form(id, window, cx)
                                            });
                                        }
                                    }))
                                    .item(PopupMenuItem::new("Delete wall").on_click({
                                        let this = this.clone();
                                        move |_, window, app| {
                                            this.update(app, |this, cx| {
                                                this.delete_view(id, window, cx)
                                            });
                                        }
                                    }))
                                }
                            })
                    })),
            )
            .when(self.library.views.is_empty(), |el| {
                el.child(
                    div()
                        .px_2()
                        .pb_2()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Open some streams, then Save wall."),
                )
            })
    }

    /// A thin strip with the window controls that are not about a wall.
    fn render_chrome(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let hidden = self.library.sidebar_hidden;

        h_flex()
            .flex_none()
            .px_1()
            .py_1()
            .gap_1()
            .items_center()
            .child(icon_button(
                "toggle-sidebar",
                "icons/panel-left.svg",
                if hidden { "Show sidebar" } else { "Hide sidebar" },
                cx,
                |this, _, cx| this.toggle_sidebar(cx),
            ))
            .child(div().flex_1())
            .child(icon_button(
                "toggle-fullscreen",
                "icons/maximize.svg",
                "Fullscreen",
                cx,
                |_, window, _| window.toggle_fullscreen(),
            ))
    }

    fn render_wall(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(view) = self.active_grid() {
            return v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(self.render_view_bar(view, cx))
                .child(self.render_view_grid(view, cx))
                .into_any_element();
        }

        let columns = grid_columns(self.open.len().max(1));

        let mut rows = Vec::new();
        for chunk in self.open.chunks(columns) {
            let mut row = h_flex().flex_1().min_h_0().w_full().overflow_hidden();
            for stream in chunk {
                row = row.child(self.render_tile(stream, cx));
            }
            // Pad the last row so a lone tile does not stretch full width.
            for filler in chunk.len()..columns {
                let _ = filler;
                row = row.child(div().flex_1().min_w_0());
            }
            rows.push(row);
        }

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .p_1()
            .gap_1()
            .when(self.open.is_empty(), |this| {
                this.child(
                    v_flex()
                        .size_full()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .child(div().text_lg().child("No streams open"))
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("Click a connection in the sidebar to start playing."),
                        ),
                )
            })
            .children(rows)
            .into_any_element()
    }

    /// Controls for the active wall. Locked by default: the size steppers and
    /// the save/cancel pair only appear once it is unlocked.
    fn render_view_bar(&self, view: &GridView, cx: &mut Context<Self>) -> impl IntoElement {
        let editing = self.editing();
        let stepper = |id: &'static str,
                       label: &'static str,
                       rows: isize,
                       cols: isize,
                       cx: &mut Context<Self>| {
            Button::new(id)
                .label(label)
                .ghost()
                .xsmall()
                .on_click(cx.listener(move |this, _, _, cx| this.resize_view(rows, cols, cx)))
        };
        let counter = |label: &'static str,
                       value: usize,
                       minus: (&'static str, isize, isize),
                       plus: (&'static str, isize, isize),
                       cx: &mut Context<Self>| {
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
                .child(stepper(minus.0, "\u{2212}", minus.1, minus.2, cx))
                .child(div().text_xs().child(value.to_string()))
                .child(stepper(plus.0, "+", plus.1, plus.2, cx))
        };

        h_flex()
            .flex_none()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().text_sm().child(view.name.clone()))
            .when(editing, |bar| {
                bar.child(
                    div()
                        .id("rename-view")
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(20.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .text_color(cx.theme().muted_foreground)
                        .hover(|style| style.bg(cx.theme().secondary))
                        .tooltip(|window, cx| {
                            gpui_component::tooltip::Tooltip::new("Rename wall").build(window, cx)
                        })
                        .child(
                            svg()
                                .path("icons/pencil.svg")
                                .size(px(13.0))
                                .flex_none()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            if let Some(id) = this.library.active_view {
                                this.open_view_form(id, window, cx);
                            }
                        })),
                )
            })
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if editing {
                        "editing".to_string()
                    } else {
                        format!("{}\u{00d7}{}", view.rows, view.cols)
                    }),
            )
            .when(editing, |bar| {
                bar.child(counter(
                    "rows",
                    view.rows,
                    ("rows-minus", -1, 0),
                    ("rows-plus", 1, 0),
                    cx,
                ))
                .child(counter(
                    "cols",
                    view.cols,
                    ("cols-minus", 0, -1),
                    ("cols-plus", 0, 1),
                    cx,
                ))
            })
            .child(div().flex_1())
            .when(!editing, |bar| {
                bar.child(
                    Button::new("edit-view")
                        .label("Edit")
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| this.begin_edit(cx))),
                )
                .child(
                    Button::new("close-view")
                        .label("Close")
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, window, cx| this.close_view(window, cx))),
                )
            })
            .when(editing, |bar| {
                bar.child(
                    Button::new("cancel-edit")
                        .label("Cancel")
                        .outline()
                        .xsmall()
                        .on_click(cx.listener(|this, _, window, cx| this.cancel_edit(window, cx))),
                )
                .child(
                    Button::new("save-edit")
                        .label("Save")
                        .primary()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| this.commit_edit(cx))),
                )
            })
    }

    /// Cells are positioned as fractions of the wall, so a cell can span rows
    /// and columns without fighting flex layout.
    fn render_view_grid(&self, view: &GridView, cx: &mut Context<Self>) -> impl IntoElement {
        let (rows, cols) = (view.rows.max(1), view.cols.max(1));
        let mut children: Vec<gpui::AnyElement> = Vec::new();

        for cell in &view.cells {
            let frame = (
                cell.col as f32 / cols as f32,
                cell.row as f32 / rows as f32,
                cell.col_span.max(1) as f32 / cols as f32,
                cell.row_span.max(1) as f32 / rows as f32,
            );
            let body = match self.open.iter().find(|s| s.id == cell.connection) {
                Some(stream) => self.render_tile(stream, cx).into_any_element(),
                None => self.render_missing_cell(cell.connection, cx),
            };
            children.push(
                self.cell_frame(frame, cell.row, cell.col, cx)
                    .child(body)
                    .into_any_element(),
            );
        }

        for row in 0..rows {
            for col in 0..cols {
                if view.cell_at(row, col).is_some() {
                    continue;
                }
                let frame = (
                    col as f32 / cols as f32,
                    row as f32 / rows as f32,
                    1.0 / cols as f32,
                    1.0 / rows as f32,
                );
                children.push(
                    self.cell_frame(frame, row, col, cx)
                        .child(
                            div()
                                .size_full()
                                .rounded(px(6.0))
                                .when(self.editing(), |slot| {
                                    slot.border_1()
                                        .border_dashed()
                                        .border_color(cx.theme().border)
                                })
                                .into_any_element(),
                        )
                        .into_any_element(),
                );
            }
        }

        div()
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .p_1()
            .children(children)
    }

    /// A positioned slot that accepts both tiles and tree rows.
    fn cell_frame(
        &self,
        frame: (f32, f32, f32, f32),
        row: usize,
        col: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (x, y, w, h) = frame;
        div()
            .id(("cell", row * 1024 + col))
            .absolute()
            .left(relative(x))
            .top(relative(y))
            .w(relative(w))
            .h(relative(h))
            .p_1()
            .flex()
            // A locked wall takes no drops at all.
            .when(self.editing(), |cell| {
                cell.drag_over::<DraggedTile>(|mut style, _, _, cx| {
                    style.background = Some(cx.theme().drop_target.into());
                    style
                })
                .drag_over::<DraggedNode>(|mut style, _, _, cx| {
                    style.background = Some(cx.theme().drop_target.into());
                    style
                })
                .on_drop::<DraggedTile>(cx.listener(
                    move |this, dragged: &DraggedTile, window, cx| {
                        this.place_in_view(dragged.id, row, col, window, cx);
                    },
                ))
                .on_drop::<DraggedNode>(cx.listener(
                    move |this, dragged: &DraggedNode, window, cx| {
                        let id = dragged.id;
                        if this.library.connection(id).is_some() {
                            this.place_in_view(id, row, col, window, cx);
                        }
                    },
                ))
            })
    }

    /// A cell whose connection has been deleted from the library.
    fn render_missing_cell(&self, id: Uuid, cx: &mut Context<Self>) -> gpui::AnyElement {
        let _ = id;
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .border_1()
            .border_color(cx.theme().border)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child("connection missing")
            .into_any_element()
    }

    fn render_tile(&self, stream: &OpenStream, cx: &mut Context<Self>) -> impl IntoElement {
        let status = stream.player.status();
        let name = stream.player.connection.name.clone();
        let id = stream.id;
        let fps = stream.player.fps();
        let status_color = match status {
            Status::Playing => cx.theme().success,
            Status::Connecting => cx.theme().muted_foreground,
            Status::Reconnecting(_) => cx.theme().warning,
            Status::Failed(_) => cx.theme().danger,
        };

        let hovered = self.hovered_tile == Some(id);

        div()
            .id(("tile", id.as_u128() as usize))
            .on_hover(cx.listener(move |this, entered: &bool, _, cx| {
                // The next tile's enter arrives before this one's leave, so a
                // tile may only clear the hover while it still owns it.
                let next = if *entered {
                    Some(id)
                } else if this.hovered_tile == Some(id) {
                    None
                } else {
                    return;
                };
                if this.hovered_tile != next {
                    this.hovered_tile = next;
                    cx.notify();
                }
            }))
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .overflow_hidden()
            .rounded(px(6.0))
            .border_1()
            .border_color(cx.theme().border)
            .bg(gpui::black())
            // The picture owns the whole tile; the header floats over it.
            .child(match stream.texture.clone() {
                Some(texture) => div()
                    .size_full()
                    .overflow_hidden()
                    .child(
                        img(texture)
                            .size_full()
                            // Contain: scaled as large as fits, never
                            // distorted and never cropped.
                            .object_fit(ObjectFit::Contain),
                    )
                    .into_any_element(),
                None => v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .p_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(status.message().to_string())
                    .into_any_element(),
            })
            .when(hovered, |tile| tile.child(
                h_flex()
                    .id(("tile-header", id.as_u128() as usize))
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .justify_between()
                    // Shaded rather than solid, so the picture still reads
                    // through it.
                    .bg(gpui::black().opacity(0.55))
                    .text_color(gpui::white())
                    .on_drag(
                        DraggedTile {
                            id,
                            label: name.clone().into(),
                        },
                        |dragged, _, _, cx| {
                            let label = dragged.label.clone();
                            cx.new(|_| DragPreview { label })
                        },
                    )
                    .drag_over::<DraggedTile>(|mut style, _, _, cx| {
                        style.background = Some(cx.theme().drop_target.into());
                        style
                    })
                    .on_drop::<DraggedTile>(cx.listener(move |this, dragged: &DraggedTile, _, cx| {
                        this.reorder_tile(dragged.id, id, cx);
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .child(div().text_sm().truncate().child(name))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(status_color)
                                    .child(if fps > 0.5 {
                                        format!("{:.0} fps", fps)
                                    } else {
                                        status.message().to_string()
                                    }),
                            ),
                    )
                    .child(
                        Button::new(("close", id.as_u128() as usize))
                            .label("\u{2715}")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_stream(id, window, cx)
                            })),
                    ),
            ))
    }

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(form) = self.form.as_ref() else {
            return div().into_any_element();
        };
        let editing = form.editing.is_some();
        let is_group = form.target != FormTarget::Connection;
        let transport = form.transport;

        let field = |label: &'static str,
                     hint: &'static str,
                     input: &Entity<InputState>,
                     tab: isize| {
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_baseline()
                        .child(div().text_sm().child(label))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(hint),
                        ),
                )
                .child(
                    div()
                        // Focus explicitly on mouse down so clicking a field
                        // always moves the caret there.
                        .on_mouse_down(gpui::MouseButton::Left, {
                            let input = input.clone();
                            move |_, window, app| {
                                input.update(app, |state, cx| state.focus(window, cx));
                            }
                        })
                        .child(Input::new(input).tab_index(tab)),
                )
        };

        div()
            .absolute()
            .inset_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.5))
            .child(
                v_flex()
                    .w(px(440.0))
                    .max_h(relative(0.9))
                    .p_4()
                    .gap_3()
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .child(div().text_lg().child(match (is_group, editing) {
                        (true, _) if form.target == FormTarget::View => "Rename wall",
                        (true, _) => "Rename group",
                        (false, true) => "Edit connection",
                        (false, false) => "New connection",
                    }))
                    .child(field("Name", "", &form.name, 1))
                    .when(!is_group, |this| {
                        this.child(field("URL", "rtsp://host:554/path", &form.url, 2))
                            .child(field("Username", "optional", &form.username, 3))
                            .child(field("Password", "optional", &form.password, 4))
                    })
                    .when(!is_group, |this| {
                        this.child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_sm().child("Transport"))
                            .child(
                                Button::new("transport-tcp")
                                    .label("TCP")
                                    .small()
                                    .when(transport == Transport::Tcp, |b| b.primary())
                                    .when(transport != Transport::Tcp, |b| b.outline())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(form) = this.form.as_mut() {
                                            form.transport = Transport::Tcp;
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("transport-udp")
                                    .label("UDP")
                                    .small()
                                    .when(transport == Transport::Udp, |b| b.primary())
                                    .when(transport != Transport::Udp, |b| b.outline())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(form) = this.form.as_mut() {
                                            form.transport = Transport::Udp;
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("cancel")
                                    .label("Cancel")
                                    .outline()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.form = None;
                                        this.error = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("save")
                                    .label("Save")
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| this.submit_form(cx))),
                            ),
                    ),
            )
            .into_any_element()
    }
}
