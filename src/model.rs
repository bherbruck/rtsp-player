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
}

impl Library {
    pub fn path() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("dev", "rtsp-player", "rtsp-player")
            .context("no home directory")?;
        Ok(dirs.config_dir().join("library.json"))
    }

    pub fn load() -> Self {
        let Ok(path) = Self::path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        // A corrupt file should not lose the user their window; start empty and
        // let the next save overwrite it.
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        // Write-then-rename so a crash mid-write cannot truncate the library.
        let tmp = path.with_extension("json.tmp");
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
