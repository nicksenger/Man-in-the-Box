use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
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
pub struct Mcts {
    config: MctsConfig,
    root: usize,
    nodes: Vec<Node>,
    index_by_key: HashMap<String, usize>,
}

impl Mcts {
    pub fn new(root_key: impl Into<String>) -> Self {
        Self::with_config(root_key, MctsConfig::default())
    }

    pub fn with_config(root_key: impl Into<String>, config: MctsConfig) -> Self {
        let root_key = root_key.into();
        let root = Node::new(root_key.clone(), None);
        let mut index_by_key = HashMap::new();
        index_by_key.insert(root_key, 0);
        Self {
            config,
            root: 0,
            nodes: vec![root],
            index_by_key,
        }
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
            }
            return Ok(());
        }

        let child_index = self.nodes.len();
        self.nodes.push(Node::new(child_key.clone(), Some(parent)));
        self.nodes[parent].children.push(child_index);
        self.index_by_key.insert(child_key, child_index);
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
        Self {
            tree: Mcts::new(root_key),
            pending_selection_path: None,
        }
    }

    pub fn with_config(root_key: impl Into<String>, config: MctsConfig) -> Self {
        Self {
            tree: Mcts::with_config(root_key, config),
            pending_selection_path: None,
        }
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
    use super::{Mcts, MctsConfig, PendingTreeSearch, normalize_reward};

    #[test]
    fn selects_root_initially() {
        let tree = Mcts::new("root");
        let path = tree.select_path(8).unwrap();
        assert_eq!(path, vec!["root".to_string()]);
    }

    #[test]
    fn progressive_widening_stops_at_expandable_parent() {
        let mut tree = Mcts::with_config(
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
        let mut tree = Mcts::with_config(
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
        let mut tree = Mcts::new("root");
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
        let mut search = PendingTreeSearch::new("root");
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
        let mut search = PendingTreeSearch::new("root");
        let step = search.select_next(16).unwrap();
        assert_eq!(step.selected_key, "root");
        assert_eq!(step.selected_path, vec!["root".to_string()]);
        assert_eq!(
            search.pending_selection_path(),
            Some(vec!["root".to_string()].as_slice())
        );
    }
}
