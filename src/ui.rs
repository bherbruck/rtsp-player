//! The window: a connection tree on the left, a video wall on the right.

use crate::model::{Connection, Library, Node, Transport};
use crate::stream::{Player, Status};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ObjectFit,
    ParentElement as _, RenderImage, SharedString, Styled as _, StyledImage as _, Task, Window,
    div, img, px, relative,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::list::ListItem;
use gpui_component::tree::{TreeItem, TreeState, tree};
use gpui_component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Roughly 30 fps. The decoders push frames into a mailbox; this is just how
/// often we look.
const REFRESH_INTERVAL: Duration = Duration::from_millis(33);

pub struct PlayerApp {
    library: Library,
    tree: Entity<TreeState>,
    selected: Option<Uuid>,
    open: Vec<OpenStream>,
    form: Option<Form>,
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
struct Form {
    editing: Option<Uuid>,
    parent: Option<Uuid>,
    /// Groups reuse this sheet but only show the name field.
    group: bool,
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

        let _ = window;
        let mut this = Self {
            library,
            tree,
            selected: None,
            open: Vec::new(),
            form: None,
            error: None,
            _refresh: refresh,
        };
        for connection in restore {
            this.open_stream(connection);
        }
        this
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

    fn on_item_click(&mut self, id: Uuid, cx: &mut Context<Self>) {
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
                self.open_stream(connection);
            }
            None => {}
        }
        cx.notify();
    }

    fn open_stream(&mut self, connection: Connection) {
        if self.open.iter().any(|s| s.id == connection.id) {
            return;
        }
        self.open.push(OpenStream {
            id: connection.id,
            player: Player::start(connection),
            texture: None,
        });
        self.save_library();
    }

    fn close_stream(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.open.iter().position(|s| s.id == id) {
            let closed = self.open.remove(ix);
            if let Some(texture) = closed.texture {
                let _ = window.drop_image(texture);
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
            group: false,
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
            group: true,
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

        if form.group {
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
            } => TreeItem::new(id.to_string(), name.clone())
                .children(tree_items(children))
                .expanded(*expanded),
            Node::Stream(connection) => {
                TreeItem::new(connection.id.to_string(), connection.name.clone())
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
            .child(self.render_sidebar(cx))
            .child(self.render_wall(cx))
            .when_some(self.form.as_ref(), |this, _| this.child(self.render_form(cx)))
    }
}

impl PlayerApp {
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_selection = self.selected.is_some();

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
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(tree(&self.tree, {
                        // The render closure only gets `&mut App`, so route
                        // clicks back through a handle to this view.
                        let this = cx.entity();
                        move |ix, entry, _selected, _window, _cx| {
                            let id = entry.item().id.parse::<Uuid>().ok();
                            let marker = if entry.is_folder() {
                                if entry.is_expanded() { "\u{25be}" } else { "\u{25b8}" }
                            } else {
                                "\u{25cf}"
                            };
                            let this = this.clone();
                            ListItem::new(ix)
                                .pl(px(8.0 + 14.0 * entry.depth() as f32))
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(div().w(px(12.0)).text_xs().child(marker))
                                        .child(entry.item().label.clone()),
                                )
                                .on_click(move |_, _window, app| {
                                    if let Some(id) = id {
                                        this.update(app, |this, cx| this.on_item_click(id, cx));
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
            .child(
                v_flex()
                    .p_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(cx.theme().sidebar_border)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("edit")
                                    .label("Edit")
                                    .outline()
                                    .small()
                                    .disabled(!has_selection)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        let Some(id) = this.selected else { return };
                                        match this.library.connection(id).cloned() {
                                            Some(connection) => {
                                                this.open_form(Some(connection), window, cx)
                                            }
                                            None => this.open_group_form(id, window, cx),
                                        }
                                    })),
                            )
                            .child(
                                Button::new("delete")
                                    .label("Delete")
                                    .danger()
                                    .small()
                                    .disabled(!has_selection)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.delete_selected(window, cx)
                                    })),
                            ),
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

    fn render_wall(&self, cx: &mut Context<Self>) -> impl IntoElement {
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

        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .overflow_hidden()
            .rounded(px(6.0))
            .border_1()
            .border_color(cx.theme().border)
            .bg(gpui::black())
            .child(
                h_flex()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .justify_between()
                    .bg(cx.theme().secondary)
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
                            .label("✕")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_stream(id, window, cx)
                            })),
                    ),
            )
            .child(match stream.texture.clone() {
                Some(texture) => div()
                    .flex_1()
                    .min_h_0()
                    .h_full()
                    .overflow_hidden()
                    .child(img(texture).size_full().object_fit(ObjectFit::Contain))
                    .into_any_element(),
                None => v_flex()
                    .flex_1()
                    .min_h_0()
                    .items_center()
                    .justify_center()
                    .p_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(status.message().to_string())
                    .into_any_element(),
            })
    }

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(form) = self.form.as_ref() else {
            return div().into_any_element();
        };
        let editing = form.editing.is_some();
        let is_group = form.group;
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
