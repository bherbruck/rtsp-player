//! Connection library: a tree of groups and RTSP connections, persisted as JSON.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// RTP interleaved over the RTSP TCP connection. Works through NAT, no packet loss.
    #[default]
    Tcp,
    /// RTP over UDP. Lower latency on a LAN, drops reordered packets.
    Udp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub id: Uuid,
    pub name: String,
    /// Full RTSP URL, without credentials (those live in `username`/`password`).
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default)]
    pub transport: Transport,
}

impl Connection {
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            url: url.into(),
            username: None,
            password: None,
            transport: Transport::default(),
        }
    }

    /// Credentials are optional; a camera with anonymous access needs none.
    pub fn credentials(&self) -> Option<(String, String)> {
        let user = self.username.as_deref().filter(|u| !u.is_empty())?;
        Some((
            user.to_string(),
            self.password.clone().unwrap_or_default(),
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Node {
    Group {
        id: Uuid,
        name: String,
        #[serde(default)]
        children: Vec<Node>,
        #[serde(default = "default_true")]
        expanded: bool,
    },
    Stream(Connection),
}

fn default_true() -> bool {
    true
}

impl Node {
    pub fn group(name: impl Into<String>) -> Self {
        Node::Group {
            id: Uuid::new_v4(),
            name: name.into(),
            children: Vec::new(),
            expanded: true,
        }
    }

    pub fn id(&self) -> Uuid {
        match self {
            Node::Group { id, .. } => *id,
            Node::Stream(c) => c.id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Node::Group { name, .. } => name,
            Node::Stream(c) => &c.name,
        }
    }

}

/// Which appearance the window uses. `System` follows the desktop setting and
/// keeps following it when the user changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePref {
    /// Cycles in the order the toggle button steps through.
    pub fn next(self) -> Self {
        match self {
            ThemePref::System => ThemePref::Light,
            ThemePref::Light => ThemePref::Dark,
            ThemePref::Dark => ThemePref::System,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            // Half-filled circle, sun, moon.
            ThemePref::System => "\u{25d0} Auto",
            ThemePref::Light => "\u{2600} Light",
            ThemePref::Dark => "\u{263e} Dark",
        }
    }
}

/// How opening a connection affects the wall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayoutMode {
    /// Opening a stream replaces whatever is playing.
    #[default]
    Single,
    /// Opening a stream adds another tile to the grid.
    Multi,
}

impl LayoutMode {
    pub fn toggled(self) -> Self {
        match self {
            LayoutMode::Single => LayoutMode::Multi,
            LayoutMode::Multi => LayoutMode::Single,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LayoutMode::Single => "One",
            LayoutMode::Multi => "Many",
        }
    }
}

fn one() -> usize {
    1
}

/// One connection placed on a saved grid. Spans let a camera take more than a
/// single cell, so a wall can have a large primary view beside small ones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridCell {
    pub row: usize,
    pub col: usize,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub row_span: usize,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub col_span: usize,
    pub connection: Uuid,
}

fn is_one(n: &usize) -> bool {
    *n == 1
}

impl GridCell {
    pub fn new(row: usize, col: usize, connection: Uuid) -> Self {
        Self {
            row,
            col,
            row_span: 1,
            col_span: 1,
            connection,
        }
    }

    pub fn covers(&self, row: usize, col: usize) -> bool {
        row >= self.row
            && row < self.row + self.row_span.max(1)
            && col >= self.col
            && col < self.col + self.col_span.max(1)
    }
}

/// A saved wall: a fixed grid with connections at chosen positions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridView {
    pub id: Uuid,
    pub name: String,
    pub rows: usize,
    pub cols: usize,
    #[serde(default)]
    pub cells: Vec<GridCell>,
}

impl GridView {
    pub fn new(name: impl Into<String>, rows: usize, cols: usize) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            rows: rows.max(1),
            cols: cols.max(1),
            cells: Vec::new(),
        }
    }

    pub fn cell_at(&self, row: usize, col: usize) -> Option<&GridCell> {
        self.cells.iter().find(|c| c.covers(row, col))
    }

    /// Puts `connection` at a position, evicting whatever occupied it.
    pub fn place(&mut self, connection: Uuid, row: usize, col: usize) {
        self.cells
            .retain(|c| c.connection != connection && !c.covers(row, col));
        self.cells.push(GridCell::new(row, col, connection));
    }

    pub fn remove(&mut self, connection: Uuid) {
        self.cells.retain(|c| c.connection != connection);
    }

    /// Drops placements that fell outside the grid after a resize, and any that
    /// a earlier cell's span already covers. Hand-edited files can overlap;
    /// without this they would be drawn on top of each other.
    pub fn clamp(&mut self) {
        self.rows = self.rows.max(1);
        self.cols = self.cols.max(1);
        let (rows, cols) = (self.rows, self.cols);
        self.cells.retain(|c| c.row < rows && c.col < cols);
        for cell in &mut self.cells {
            cell.row_span = cell.row_span.max(1).min(rows - cell.row);
            cell.col_span = cell.col_span.max(1).min(cols - cell.col);
        }

        let mut kept: Vec<GridCell> = Vec::with_capacity(self.cells.len());
        for cell in std::mem::take(&mut self.cells) {
            let overlaps = kept.iter().any(|k| {
                (cell.row..cell.row + cell.row_span).any(|r| {
                    (cell.col..cell.col + cell.col_span).any(|c| k.covers(r, c))
                })
            });
            if !overlaps {
                kept.push(cell);
            }
        }
        self.cells = kept;
    }

}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Library {
    #[serde(default)]
    pub roots: Vec<Node>,
    /// Streams that were playing when the window last closed, reopened on
    /// launch so a monitoring wall comes back the way it was left.
    #[serde(default)]
    pub open: Vec<Uuid>,
    #[serde(default)]
    pub theme: ThemePref,
    #[serde(default)]
    pub layout: LayoutMode,
    /// Collapsed sidebar, so a wall can fill the window.
    #[serde(default)]
    pub sidebar_hidden: bool,
    /// Saved walls, each a fixed grid with connections at chosen positions.
    #[serde(default)]
    pub views: Vec<GridView>,
    /// The view currently on the wall, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_view: Option<Uuid>,
}

impl Library {
    /// Makes a hand-edited file safe to render.
    fn sanitize(&mut self) {
        for view in &mut self.views {
            view.clamp();
        }
        let known: Vec<Uuid> = self.views.iter().map(|v| v.id).collect();
        if let Some(active) = self.active_view
            && !known.contains(&active)
        {
            self.active_view = None;
        }
    }

    pub fn view(&self, id: Uuid) -> Option<&GridView> {
        self.views.iter().find(|v| v.id == id)
    }

    pub fn view_mut(&mut self, id: Uuid) -> Option<&mut GridView> {
        self.views.iter_mut().find(|v| v.id == id)
    }
}

impl Library {
    /// The config file. `RTSP_PLAYER_CONFIG` overrides it, which is handy for
    /// keeping several libraries or pointing at one in a repo.
    pub fn path() -> Result<PathBuf> {
        if let Some(override_path) = std::env::var_os("RTSP_PLAYER_CONFIG") {
            return Ok(PathBuf::from(override_path));
        }
        let dirs = directories::ProjectDirs::from("dev", "rtsp-player", "rtsp-player")
            .context("no home directory")?;
        Ok(dirs.config_dir().join("library.yaml"))
    }

    /// Where a pre-YAML config would have been written.
    fn legacy_json_path() -> Result<PathBuf> {
        Ok(Self::path()?.with_extension("json"))
    }

    pub fn load() -> Self {
        let Ok(path) = Self::path() else {
            return Self::default();
        };

        if let Ok(text) = std::fs::read_to_string(&path) {
            // A corrupt file should not lose the user their window; start empty
            // and let the next save overwrite it.
            let mut library: Self = serde_yaml_ng::from_str(&text).unwrap_or_default();
            library.sanitize();
            return library;
        }

        // First run after the switch to YAML: pick up the old file.
        let Ok(legacy) = Self::legacy_json_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&legacy) else {
            return Self::default();
        };
        let library: Self = serde_json::from_str(&text).unwrap_or_default();
        if library.save().is_ok() {
            // Keep the original around rather than deleting the user's data.
            let _ = std::fs::rename(&legacy, legacy.with_extension("json.bak"));
        }
        library
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_yaml_ng::to_string(self)?;
        // Write-then-rename so a crash mid-write cannot truncate the library.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn find(&self, id: Uuid) -> Option<&Node> {
        fn walk(nodes: &[Node], id: Uuid) -> Option<&Node> {
            for node in nodes {
                if node.id() == id {
                    return Some(node);
                }
                if let Node::Group { children, .. } = node
                    && let Some(found) = walk(children, id)
                {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.roots, id)
    }

    pub fn connection(&self, id: Uuid) -> Option<&Connection> {
        match self.find(id)? {
            Node::Stream(c) => Some(c),
            Node::Group { .. } => None,
        }
    }

    /// Insert `node` into the group `parent`, or at the root when `parent` is
    /// `None` or names something that is not a group.
    pub fn insert(&mut self, parent: Option<Uuid>, node: Node) {
        fn walk(nodes: &mut [Node], parent: Uuid, node: Node) -> Option<Node> {
            let mut node = node;
            for existing in nodes.iter_mut() {
                let Node::Group { id, children, .. } = existing else {
                    continue;
                };
                if *id == parent {
                    children.push(node);
                    return None;
                }
                match walk(children, parent, node) {
                    // Not in this subtree; carry the node on to the next sibling.
                    Some(returned) => node = returned,
                    None => return None,
                }
            }
            Some(node)
        }

        let leftover = match parent {
            Some(parent) => walk(&mut self.roots, parent, node),
            None => Some(node),
        };
        if let Some(node) = leftover {
            self.roots.push(node);
        }
    }

    pub fn remove(&mut self, id: Uuid) -> Option<Node> {
        fn walk(nodes: &mut Vec<Node>, id: Uuid) -> Option<Node> {
            if let Some(ix) = nodes.iter().position(|n| n.id() == id) {
                return Some(nodes.remove(ix));
            }
            for node in nodes.iter_mut() {
                if let Node::Group { children, .. } = node
                    && let Some(found) = walk(children, id)
                {
                    return Some(found);
                }
            }
            None
        }
        walk(&mut self.roots, id)
    }

    pub fn update_connection(&mut self, updated: Connection) {
        fn walk(nodes: &mut [Node], updated: &Connection) -> bool {
            for node in nodes.iter_mut() {
                let found = match node {
                    Node::Stream(c) if c.id == updated.id => {
                        *c = updated.clone();
                        true
                    }
                    Node::Group { children, .. } => walk(children, updated),
                    Node::Stream(_) => false,
                };
                if found {
                    return true;
                }
            }
            false
        }
        walk(&mut self.roots, &updated);
    }

    pub fn rename_group(&mut self, id: Uuid, new_name: String) {
        fn walk(nodes: &mut [Node], id: Uuid, new_name: &str) -> bool {
            for node in nodes.iter_mut() {
                if let Node::Group {
                    id: gid,
                    name,
                    children,
                    ..
                } = node
                {
                    if *gid == id {
                        *name = new_name.to_string();
                        return true;
                    }
                    if walk(children, id, new_name) {
                        return true;
                    }
                }
            }
            false
        }
        walk(&mut self.roots, id, &new_name);
    }

    pub fn set_expanded(&mut self, id: Uuid, value: bool) {
        fn walk(nodes: &mut [Node], id: Uuid, value: bool) -> bool {
            for node in nodes.iter_mut() {
                if let Node::Group {
                    id: gid,
                    expanded,
                    children,
                    ..
                } = node
                {
                    if *gid == id {
                        *expanded = value;
                        return true;
                    }
                    if walk(children, id, value) {
                        return true;
                    }
                }
            }
            false
        }
        walk(&mut self.roots, id, value);
    }

    /// Every connection at or below `id`, depth first, so a group can be
    /// opened as a set.
    pub fn connections_under(&self, id: Uuid) -> Vec<Connection> {
        fn collect(node: &Node, out: &mut Vec<Connection>) {
            match node {
                Node::Stream(c) => out.push(c.clone()),
                Node::Group { children, .. } => {
                    for child in children {
                        collect(child, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        if let Some(node) = self.find(id) {
            collect(node, &mut out);
        }
        out
    }

    /// Reparent `node` under `new_parent`, or to the top level when `None`.
    /// Returns false when the move is not allowed or changes nothing.
    pub fn move_node(&mut self, node: Uuid, new_parent: Option<Uuid>) -> bool {
        if let Some(parent) = new_parent {
            // Dropping a group into itself or one of its own descendants would
            // detach that whole subtree from the tree.
            if self.is_ancestor(node, parent) {
                return false;
            }
        }
        if self.parent_of(node) == new_parent {
            return false;
        }
        let Some(detached) = self.remove(node) else {
            return false;
        };
        self.insert(new_parent, detached);
        true
    }

    /// The group holding `child`, or `None` when it sits at the top level.
    pub fn parent_of(&self, child: Uuid) -> Option<Uuid> {
        fn walk(nodes: &[Node], child: Uuid) -> Option<Uuid> {
            for node in nodes {
                let Node::Group { id, children, .. } = node else {
                    continue;
                };
                if children.iter().any(|c| c.id() == child) {
                    return Some(*id);
                }
                if let Some(found) = walk(children, child) {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.roots, child)
    }

    /// True when `ancestor` is `node` or contains it; used to reject moves that
    /// would drop a group inside itself.
    pub fn is_ancestor(&self, ancestor: Uuid, node: Uuid) -> bool {
        fn contains(node: &Node, target: Uuid) -> bool {
            if node.id() == target {
                return true;
            }
            match node {
                Node::Group { children, .. } => children.iter().any(|c| contains(c, target)),
                Node::Stream(_) => false,
            }
        }
        self.find(ancestor)
            .map(|n| contains(n, node))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(name: &str, expanded: bool, children: Vec<Node>) -> Node {
        Node::Group {
            id: Uuid::new_v4(),
            name: name.into(),
            children,
            expanded,
        }
    }

    fn library() -> Library {
        Library {
            roots: vec![
                group("open", true, vec![Node::Stream(Connection::new("a", "rtsp://a"))]),
                group("shut", false, vec![Node::Stream(Connection::new("b", "rtsp://b"))]),
                Node::Stream(Connection::new("loose", "rtsp://c")),
            ],
            ..Default::default()
        }
    }

    fn expansion(library: &Library) -> Vec<(String, bool)> {
        fn walk(nodes: &[Node], out: &mut Vec<(String, bool)>) {
            for node in nodes {
                if let Node::Group {
                    name,
                    expanded,
                    children,
                    ..
                } = node
                {
                    out.push((name.clone(), *expanded));
                    walk(children, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&library.roots, &mut out);
        out
    }

    /// Dragging a node about must not open or close anything.
    #[test]
    fn moving_a_node_leaves_expansion_alone() {
        let mut library = library();
        let before = expansion(&library);

        let loose = library.roots[2].id();
        let shut = library.roots[1].id();
        assert!(library.move_node(loose, Some(shut)));

        assert_eq!(expansion(&library), before);
        assert_eq!(library.parent_of(loose), Some(shut));
    }

    #[test]
    fn a_group_cannot_be_dropped_inside_itself() {
        let mut library = library();
        let outer = library.roots[0].id();
        let inner = match &library.roots[0] {
            Node::Group { children, .. } => children[0].id(),
            _ => unreachable!(),
        };
        assert!(!library.move_node(outer, Some(inner)));
        assert!(!library.move_node(outer, Some(outer)));
    }

    /// A span that covers its neighbours wins; the covered cells are dropped.
    #[test]
    fn overlapping_cells_are_resolved_on_clamp() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut view = GridView::new("wall", 3, 3);
        view.cells.push(GridCell {
            row: 0,
            col: 0,
            row_span: 2,
            col_span: 2,
            connection: a,
        });
        view.cells.push(GridCell::new(1, 1, b));
        view.clamp();
        assert_eq!(view.cells.len(), 1);
        assert_eq!(view.cells[0].connection, a);
    }
}
