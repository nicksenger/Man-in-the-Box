use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub const DEFAULT_MCTS_PERSISTENCE_PATH: &str = "./.mitb/mcts.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MctsConfig {
    pub exploration_constant: f64,
    pub progressive_widening_k: f64,
    pub progressive_widening_alpha: f64,
}

impl Default for MctsConfig {
    fn default() -> Self {
        Self {
            exploration_constant: std::f64::consts::SQRT_2,
            progressive_widening_k: 1.5,
            progressive_widening_alpha: 0.5,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NodeStats {
    pub visits: u64,
    pub mean_reward: f64,
    pub best_reward: f64,
    pub children: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct Node {
    key: String,
    parent: Option<usize>,
    children: Vec<usize>,
    visits: u64,
    reward_sum: f64,
    best_reward: f64,
}

impl Node {
    fn new(key: String, parent: Option<usize>) -> Self {
        Self {
            key,
            parent,
            children: Vec::new(),
            visits: 0,
            reward_sum: 0.0,
            best_reward: f64::NEG_INFINITY,
        }
    }

    fn mean_reward(&self) -> f64 {
        if self.visits == 0 {
            0.0
        } else {
            self.reward_sum / self.visits as f64
        }
    }
}

#[derive(Clone, Debug)]
pub struct MctsBuilder {
    root_key: String,
    config: MctsConfig,
    persistence_path: Option<PathBuf>,
}

impl MctsBuilder {
    pub fn new(root_key: impl Into<String>) -> Self {
        Self {
            root_key: root_key.into(),
            config: MctsConfig::default(),
            persistence_path: None,
        }
    }

    pub fn config(mut self, config: MctsConfig) -> Self {
        self.config = config;
        self
    }

    pub fn persistence_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.persistence_path = Some(path.into());
        self
    }

    pub fn enable_persistence(mut self) -> Self {
        self.persistence_path = Some(PathBuf::from(DEFAULT_MCTS_PERSISTENCE_PATH));
        self
    }

    pub fn disable_persistence(mut self) -> Self {
        self.persistence_path = None;
        self
    }

    pub fn build(self) -> Mcts {
        let root_key = self.root_key.clone();
        let config = self.config.clone();
        let persistence_path = self.persistence_path.clone();
        match self.try_build() {
            Ok(tree) => tree,
            Err(_) => Mcts::fresh(root_key, config, persistence_path),
        }
    }

    pub fn try_build(self) -> Result<Mcts, String> {
        if let Some(path) = self.persistence_path.as_ref()
            && let Some(mut loaded) = Mcts::maybe_load_from_disk(path.as_path())?
            && loaded.root_key() == self.root_key
        {
            loaded.config = self.config;
            loaded.persistence_path = Some(path.clone());
            return Ok(loaded);
        }

        Ok(Mcts::fresh(
            self.root_key,
            self.config,
            self.persistence_path,
        ))
    }
}

#[derive(Clone, Debug)]
pub struct Mcts {
    config: MctsConfig,
    root: usize,
    nodes: Vec<Node>,
    index_by_key: HashMap<String, usize>,
    persistence_path: Option<PathBuf>,
}

impl Mcts {
    pub fn builder(root_key: impl Into<String>) -> MctsBuilder {
        MctsBuilder::new(root_key)
    }

    pub fn default_persistence_path() -> &'static str {
        DEFAULT_MCTS_PERSISTENCE_PATH
    }

    pub fn new(root_key: impl Into<String>) -> Self {
        Self::builder(root_key).build()
    }

    pub fn with_config(root_key: impl Into<String>, config: MctsConfig) -> Self {
        Self::builder(root_key).config(config).build()
    }

    pub fn persistence_path(&self) -> Option<&Path> {
        self.persistence_path.as_deref()
    }

    pub fn flush_persistence(&self) -> Result<(), String> {
        self.persist_to_disk()
    }

    pub fn root_key(&self) -> &str {
        self.nodes[self.root].key.as_str()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.index_by_key.contains_key(key)
    }

    pub fn ensure_child(
        &mut self,
        parent_key: &str,
        child_key: impl Into<String>,
    ) -> Result<(), String> {
        let parent = self
            .index_by_key
            .get(parent_key)
            .copied()
            .ok_or_else(|| format!("unknown parent node `{parent_key}`"))?;
        let child_key = child_key.into();

        if let Some(existing_child) = self.index_by_key.get(child_key.as_str()).copied() {
            let existing_parent = self.nodes[existing_child].parent;
            if existing_parent != Some(parent) {
                return Err(format!(
                    "node `{child_key}` already exists under a different parent"
                ));
            }
            if !self.nodes[parent].children.contains(&existing_child) {
                self.nodes[parent].children.push(existing_child);
                self.persist_best_effort();
            }
            return Ok(());
        }

        let child_index = self.nodes.len();
        self.nodes.push(Node::new(child_key.clone(), Some(parent)));
        self.nodes[parent].children.push(child_index);
        self.index_by_key.insert(child_key, child_index);
        self.persist_best_effort();
        Ok(())
    }

    pub fn backpropagate_path(&mut self, path: &[String], reward: f64) -> Result<(), String> {
        if !reward.is_finite() {
            return Err(String::from("reward must be finite"));
        }
        if path.is_empty() {
            return Err(String::from("path must include at least one node"));
        }

        for index in 0..path.len() {
            let key = path[index].as_str();
            let node_index = self
                .index_by_key
                .get(key)
                .copied()
                .ok_or_else(|| format!("unknown node `{key}` in backpropagation path"))?;

            if index > 0 {
                let previous_key = path[index - 1].as_str();
                let previous_index = self
                    .index_by_key
                    .get(previous_key)
                    .copied()
                    .ok_or_else(|| format!("unknown node `{previous_key}` in path"))?;
                if !self.nodes[previous_index].children.contains(&node_index) {
                    return Err(format!(
                        "invalid path segment `{previous_key}` -> `{key}`: child relationship not found"
                    ));
                }
            }

            let node = &mut self.nodes[node_index];
            node.visits = node.visits.saturating_add(1);
            node.reward_sum += reward;
            if reward > node.best_reward {
                node.best_reward = reward;
            }
        }

        self.persist_best_effort();
        Ok(())
    }

    pub fn select_path(&self, max_depth: usize) -> Result<Vec<String>, String> {
        if max_depth == 0 {
            return Err(String::from("max_depth must be at least 1"));
        }

        let mut selected = vec![self.nodes[self.root].key.clone()];
        let mut cursor = self.root;

        while selected.len() < max_depth {
            if self.should_expand_here(cursor) {
                break;
            }

            let next_child = self.select_best_child_by_ucb(cursor).ok_or_else(|| {
                format!(
                    "node `{}` had no selectable children",
                    self.nodes[cursor].key
                )
            })?;

            selected.push(self.nodes[next_child].key.clone());
            cursor = next_child;
        }

        Ok(selected)
    }

    pub fn node_stats(&self, key: &str) -> Option<NodeStats> {
        let index = self.index_by_key.get(key).copied()?;
        let node = &self.nodes[index];
        Some(NodeStats {
            visits: node.visits,
            mean_reward: node.mean_reward(),
            best_reward: if node.best_reward.is_finite() {
                node.best_reward
            } else {
                0.0
            },
            children: node.children.len(),
        })
    }

    fn fresh(root_key: String, config: MctsConfig, persistence_path: Option<PathBuf>) -> Self {
        let root = Node::new(root_key.clone(), None);
        let mut index_by_key = HashMap::new();
        index_by_key.insert(root_key, 0);
        Self {
            config,
            root: 0,
            nodes: vec![root],
            index_by_key,
            persistence_path,
        }
    }

    fn should_expand_here(&self, node_index: usize) -> bool {
        let node = &self.nodes[node_index];
        if node.children.is_empty() {
            return true;
        }

        let visits = node.visits.max(1) as f64;
        let widening_limit = self.config.progressive_widening_k
            * visits.powf(self.config.progressive_widening_alpha);
        (node.children.len() as f64) < widening_limit
    }

    fn select_best_child_by_ucb(&self, parent_index: usize) -> Option<usize> {
        let parent = &self.nodes[parent_index];
        if parent.children.is_empty() {
            return None;
        }

        let parent_visits = parent.visits.max(1) as f64;
        let ln_parent_visits = parent_visits.ln();
        let exploration_constant = self.config.exploration_constant;

        let mut best_child = None;
        let mut best_score = f64::NEG_INFINITY;

        for child_index in parent.children.iter().copied() {
            let child = &self.nodes[child_index];
            let score = if child.visits == 0 {
                f64::INFINITY
            } else {
                let exploitation = child.mean_reward();
                let exploration =
                    exploration_constant * (ln_parent_visits / child.visits as f64).sqrt();
                exploitation + exploration
            };

            if score > best_score {
                best_score = score;
                best_child = Some(child_index);
                continue;
            }

            if (score == best_score)
                && best_child
                    .map(|existing| self.nodes[child_index].key < self.nodes[existing].key)
                    .unwrap_or(false)
            {
                best_child = Some(child_index);
            }
        }

        best_child
    }

    fn maybe_load_from_disk(path: &Path) -> Result<Option<Self>, String> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "failed reading MCTS persistence file `{}`: {error}",
                    path.display()
                ));
            }
        };

        let persisted: PersistedMcts =
            serde_json::from_str(contents.as_str()).map_err(|error| {
                format!(
                    "failed parsing MCTS persistence file `{}`: {error}",
                    path.display()
                )
            })?;
        let tree = Self::from_persisted(persisted, Some(path.to_path_buf()))?;
        Ok(Some(tree))
    }

    fn from_persisted(
        persisted: PersistedMcts,
        persistence_path: Option<PathBuf>,
    ) -> Result<Self, String> {
        if persisted.nodes.is_empty() {
            return Err(String::from("persisted MCTS tree had no nodes"));
        }
        if persisted.root >= persisted.nodes.len() {
            return Err(format!(
                "persisted MCTS root index {} was out of bounds for {} nodes",
                persisted.root,
                persisted.nodes.len()
            ));
        }

        let mut nodes = Vec::with_capacity(persisted.nodes.len());
        for node in persisted.nodes {
            nodes.push(node.into_node());
        }

        if nodes[persisted.root].parent.is_some() {
            return Err(String::from("persisted MCTS root node had a parent"));
        }

        let mut index_by_key = HashMap::new();
        for (index, node) in nodes.iter().enumerate() {
            if !node.reward_sum.is_finite() {
                return Err(format!(
                    "persisted MCTS node `{}` had non-finite reward sum",
                    node.key
                ));
            }
            if !node.best_reward.is_finite() && node.best_reward != f64::NEG_INFINITY {
                return Err(format!(
                    "persisted MCTS node `{}` had invalid best reward",
                    node.key
                ));
            }
            if index_by_key.insert(node.key.clone(), index).is_some() {
                return Err(format!(
                    "persisted MCTS tree had duplicate node key `{}`",
                    node.key
                ));
            }
        }

        for (index, node) in nodes.iter().enumerate() {
            if let Some(parent) = node.parent {
                if parent >= nodes.len() {
                    return Err(format!(
                        "persisted MCTS node `{}` referenced parent index {} out of bounds",
                        node.key, parent
                    ));
                }
                if !nodes[parent].children.contains(&index) {
                    return Err(format!(
                        "persisted MCTS parent-child relationship missing for `{}`",
                        node.key
                    ));
                }
            }

            for child in node.children.iter().copied() {
                if child >= nodes.len() {
                    return Err(format!(
                        "persisted MCTS node `{}` referenced child index {} out of bounds",
                        node.key, child
                    ));
                }
                if nodes[child].parent != Some(index) {
                    return Err(format!(
                        "persisted MCTS child-parent mismatch for `{}` -> `{}`",
                        node.key, nodes[child].key
                    ));
                }
            }
        }

        Ok(Self {
            config: persisted.config,
            root: persisted.root,
            nodes,
            index_by_key,
            persistence_path,
        })
    }

    fn to_persisted(&self) -> PersistedMcts {
        let nodes = self
            .nodes
            .iter()
            .map(PersistedNode::from_node)
            .collect::<Vec<_>>();
        PersistedMcts {
            config: self.config.clone(),
            root: self.root,
            nodes,
        }
    }

    fn persist_to_disk(&self) -> Result<(), String> {
        let Some(path) = self.persistence_path.as_ref() else {
            return Ok(());
        };

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "failed creating MCTS persistence directory `{}`: {error}",
                    parent.display()
                )
            })?;
        }

        let serialized = serde_json::to_vec_pretty(&self.to_persisted()).map_err(|error| {
            format!(
                "failed serializing MCTS persistence file `{}`: {error}",
                path.display()
            )
        })?;
        fs::write(path, serialized).map_err(|error| {
            format!(
                "failed writing MCTS persistence file `{}`: {error}",
                path.display()
            )
        })?;
        Ok(())
    }

    fn persist_best_effort(&self) {
        let _ = self.persist_to_disk();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedMcts {
    config: MctsConfig,
    root: usize,
    nodes: Vec<PersistedNode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedNode {
    key: String,
    parent: Option<usize>,
    children: Vec<usize>,
    visits: u64,
    reward_sum: f64,
    best_reward: Option<f64>,
}

impl PersistedNode {
    fn from_node(node: &Node) -> Self {
        Self {
            key: node.key.clone(),
            parent: node.parent,
            children: node.children.clone(),
            visits: node.visits,
            reward_sum: node.reward_sum,
            best_reward: if node.best_reward.is_finite() {
                Some(node.best_reward)
            } else {
                None
            },
        }
    }

    fn into_node(self) -> Node {
        Node {
            key: self.key,
            parent: self.parent,
            children: self.children,
            visits: self.visits,
            reward_sum: self.reward_sum,
            best_reward: match self.best_reward {
                Some(best_reward) => best_reward,
                None => f64::NEG_INFINITY,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelectionStep {
    pub selected_key: String,
    pub selected_path: Vec<String>,
    pub backpropagation_path: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct PendingTreeSearch {
    tree: Mcts,
    pending_selection_path: Option<Vec<String>>,
}

impl PendingTreeSearch {
    pub fn new(root_key: impl Into<String>) -> Self {
        Self::with_tree(Mcts::new(root_key))
    }

    pub fn with_tree(tree: Mcts) -> Self {
        Self {
            tree,
            pending_selection_path: None,
        }
    }

    pub fn with_config(root_key: impl Into<String>, config: MctsConfig) -> Self {
        Self::with_tree(Mcts::with_config(root_key, config))
    }

    pub fn tree(&self) -> &Mcts {
        &self.tree
    }

    pub fn tree_mut(&mut self) -> &mut Mcts {
        &mut self.tree
    }

    pub fn pending_selection_path(&self) -> Option<&[String]> {
        self.pending_selection_path.as_deref()
    }

    pub fn select_next(&mut self, max_depth: usize) -> Result<SelectionStep, String> {
        let selected_path = self.tree.select_path(max_depth)?;
        let selected_key = selected_path
            .last()
            .cloned()
            .ok_or_else(|| String::from("mcts returned an empty selection path"))?;
        self.pending_selection_path = Some(selected_path.clone());
        Ok(SelectionStep {
            selected_key,
            selected_path,
            backpropagation_path: Vec::new(),
        })
    }

    pub fn backpropagate_and_select(
        &mut self,
        current_key: &str,
        reward: f64,
        max_depth: usize,
    ) -> Result<SelectionStep, String> {
        let backpropagation_path = if let Some(previous_path) = self.pending_selection_path.as_ref()
        {
            let mut path = previous_path.clone();
            let parent = path
                .last()
                .cloned()
                .ok_or_else(|| String::from("pending selection path was unexpectedly empty"))?;
            if parent != current_key {
                self.tree
                    .ensure_child(parent.as_str(), current_key.to_string())?;
                path.push(current_key.to_string());
            }
            path
        } else {
            let root = self.tree.root_key().to_string();
            if root != current_key {
                self.tree
                    .ensure_child(root.as_str(), current_key.to_string())?;
                vec![root, current_key.to_string()]
            } else {
                vec![root]
            }
        };

        self.tree
            .backpropagate_path(&backpropagation_path, reward)?;
        let selected_path = self.tree.select_path(max_depth)?;
        let selected_key = selected_path
            .last()
            .cloned()
            .ok_or_else(|| String::from("mcts returned an empty selection path"))?;
        self.pending_selection_path = Some(selected_path.clone());

        Ok(SelectionStep {
            selected_key,
            selected_path,
            backpropagation_path,
        })
    }
}

pub fn normalize_reward(reward: f64) -> f64 {
    if reward.is_finite() {
        reward.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_MCTS_PERSISTENCE_PATH, Mcts, MctsConfig, PendingTreeSearch, normalize_reward,
    };
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn in_memory_tree(root_key: &str) -> Mcts {
        Mcts::builder(root_key).disable_persistence().build()
    }

    fn in_memory_tree_with_config(root_key: &str, config: MctsConfig) -> Mcts {
        Mcts::builder(root_key)
            .config(config)
            .disable_persistence()
            .build()
    }

    fn in_memory_search(root_key: &str) -> PendingTreeSearch {
        PendingTreeSearch::with_tree(in_memory_tree(root_key))
    }

    fn unique_persistence_path(test_name: &str) -> PathBuf {
        let nanos = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_nanos(),
            Err(_) => 0,
        };
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mitb-mcts-{test_name}-{}-{nanos}",
            std::process::id()
        ));
        path.push("mcts.json");
        path
    }

    fn cleanup_persistence_path(path: &Path) {
        let _ = std::fs::remove_file(path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }

    #[test]
    fn default_persistence_path_matches_expected_location() {
        assert_eq!(
            Mcts::default_persistence_path(),
            DEFAULT_MCTS_PERSISTENCE_PATH
        );
    }

    #[test]
    fn builder_is_non_persistent_by_default() {
        let tree = Mcts::builder("root").build();
        assert_eq!(tree.persistence_path(), None);
    }

    #[test]
    fn builder_can_enable_default_persistence_path() {
        let tree = Mcts::builder("root").enable_persistence().build();
        assert_eq!(
            tree.persistence_path(),
            Some(Path::new(DEFAULT_MCTS_PERSISTENCE_PATH))
        );
    }

    #[test]
    fn builder_can_disable_persistence() {
        let tree = Mcts::builder("root")
            .enable_persistence()
            .disable_persistence()
            .build();
        assert_eq!(tree.persistence_path(), None);
    }

    #[test]
    fn persists_and_restores_tree_to_custom_path() {
        let path = unique_persistence_path("restore");
        let mut tree = Mcts::builder("root").persistence_path(path.clone()).build();
        tree.ensure_child("root", "a").unwrap();
        tree.backpropagate_path(&["root".to_string(), "a".to_string()], 0.7)
            .unwrap();
        tree.flush_persistence().unwrap();

        let restored = Mcts::builder("root")
            .persistence_path(path.clone())
            .try_build()
            .unwrap();
        assert!(restored.contains("a"));
        let stats = restored.node_stats("a").unwrap();
        assert_eq!(stats.visits, 1);
        assert!((stats.mean_reward - 0.7).abs() < 1e-9);

        cleanup_persistence_path(path.as_path());
    }

    #[test]
    fn selects_root_initially() {
        let tree = in_memory_tree("root");
        let path = tree.select_path(8).unwrap();
        assert_eq!(path, vec!["root".to_string()]);
    }

    #[test]
    fn progressive_widening_stops_at_expandable_parent() {
        let mut tree = in_memory_tree_with_config(
            "root",
            MctsConfig {
                exploration_constant: std::f64::consts::SQRT_2,
                progressive_widening_k: 2.0,
                progressive_widening_alpha: 0.5,
            },
        );
        tree.ensure_child("root", "a").unwrap();
        tree.backpropagate_path(&["root".to_string(), "a".to_string()], 0.8)
            .unwrap();

        let path = tree.select_path(8).unwrap();
        assert_eq!(path, vec!["root".to_string()]);
    }

    #[test]
    fn ucb_descends_when_parent_is_not_expandable() {
        let mut tree = in_memory_tree_with_config(
            "root",
            MctsConfig {
                exploration_constant: 0.0,
                progressive_widening_k: 0.0,
                progressive_widening_alpha: 0.5,
            },
        );
        tree.ensure_child("root", "a").unwrap();
        tree.ensure_child("root", "b").unwrap();
        tree.backpropagate_path(&["root".to_string(), "a".to_string()], 1.0)
            .unwrap();
        tree.backpropagate_path(&["root".to_string(), "a".to_string()], 1.0)
            .unwrap();
        tree.backpropagate_path(&["root".to_string(), "b".to_string()], 0.1)
            .unwrap();

        let path = tree.select_path(8).unwrap();
        assert_eq!(path, vec!["root".to_string(), "a".to_string()]);
    }

    #[test]
    fn backpropagate_updates_visit_and_reward_stats() {
        let mut tree = in_memory_tree("root");
        tree.ensure_child("root", "a").unwrap();
        tree.backpropagate_path(&["root".to_string(), "a".to_string()], 0.5)
            .unwrap();
        tree.backpropagate_path(&["root".to_string(), "a".to_string()], 1.0)
            .unwrap();

        let root = tree.node_stats("root").unwrap();
        let a = tree.node_stats("a").unwrap();

        assert_eq!(root.visits, 2);
        assert_eq!(a.visits, 2);
        assert!((a.mean_reward - 0.75).abs() < 1e-9);
        assert!((a.best_reward - 1.0).abs() < 1e-9);
    }

    #[test]
    fn normalize_reward_clamps_to_bounded_range() {
        assert!((normalize_reward(0.4) - 0.4).abs() < 1e-12);
        assert!((normalize_reward(2.0) - 1.0).abs() < 1e-12);
        assert!((normalize_reward(-2.0) - 0.0).abs() < 1e-12);
        assert!((normalize_reward(f64::NAN) - 0.0).abs() < 1e-12);
    }

    #[test]
    fn pending_search_backpropagates_and_selects() {
        let mut search = in_memory_search("root");
        search.tree_mut().ensure_child("root", "a").unwrap();
        search.pending_selection_path = Some(vec!["root".to_string(), "a".to_string()]);

        let step = search.backpropagate_and_select("child", 0.9, 16).unwrap();

        assert_eq!(
            step.backpropagation_path,
            vec!["root".to_string(), "a".to_string(), "child".to_string()]
        );
        assert_eq!(step.selected_path.first().map(String::as_str), Some("root"));
        assert!(search.tree().contains("child"));
    }

    #[test]
    fn pending_search_select_next_tracks_path() {
        let mut search = in_memory_search("root");
        let step = search.select_next(16).unwrap();
        assert_eq!(step.selected_key, "root");
        assert_eq!(step.selected_path, vec!["root".to_string()]);
        assert_eq!(
            search.pending_selection_path(),
            Some(vec!["root".to_string()].as_slice())
        );
    }
}
