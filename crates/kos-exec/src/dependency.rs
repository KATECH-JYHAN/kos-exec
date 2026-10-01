// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::{HashMap, HashSet, VecDeque};

use crate::config::AppConfig;
use crate::error::{KosError, Result};

#[derive(Debug)]
pub struct DependencyResolver {
    graph: HashMap<String, Vec<String>>,
}

impl DependencyResolver {
    pub fn build(apps: &[AppConfig]) -> Result<Self> {
        let known: HashSet<&str> = apps.iter().map(|a| a.id.as_str()).collect();
        let mut graph = HashMap::new();

        for app in apps {
            for dep in &app.depends_on {
                if !known.contains(dep.as_str()) {
                    return Err(KosError::NotFound(format!(
                        "dependency {dep} of app {}",
                        app.id
                    )));
                }
            }
            graph.insert(app.id.clone(), app.depends_on.clone());
        }

        Ok(Self { graph })
    }

    pub fn resolve_order(&self) -> Result<Vec<Vec<String>>> {
        let mut in_degree: HashMap<&str, usize> = HashMap::new();
        let mut dependants: HashMap<&str, Vec<&str>> = HashMap::new();

        for (node, deps) in &self.graph {
            in_degree.entry(node.as_str()).or_insert(0);
            for dep in deps {
                *in_degree.entry(node.as_str()).or_insert(0) += 1;
                dependants
                    .entry(dep.as_str())
                    .or_default()
                    .push(node.as_str());
                in_degree.entry(dep.as_str()).or_insert(0);
            }
        }

        let mut queue: VecDeque<&str> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(&n, _)| n)
            .collect();

        let mut layers: Vec<Vec<String>> = Vec::new();
        let mut visited = 0usize;

        while !queue.is_empty() {
            let mut layer: Vec<String> = Vec::new();
            let mut next_queue: VecDeque<&str> = VecDeque::new();

            for node in queue.drain(..) {
                visited += 1;
                layer.push(node.to_string());

                if let Some(deps) = dependants.get(node) {
                    for &dep in deps {
                        let deg = in_degree.get_mut(dep).unwrap();
                        *deg -= 1;
                        if *deg == 0 {
                            next_queue.push_back(dep);
                        }
                    }
                }
            }

            layer.sort();
            layers.push(layer);
            queue = next_queue;
        }

        if visited != self.graph.len() {
            let cycle: Vec<String> = self
                .graph
                .keys()
                .filter(|k| in_degree.get(k.as_str()).copied().unwrap_or(0) > 0)
                .cloned()
                .collect();
            return Err(KosError::InvalidConfig(format!(
                "circular dependency: {:?}",
                cycle
            )));
        }

        Ok(layers)
    }

    pub fn check_circular(&self) -> Option<Vec<String>> {
        match self.resolve_order() {
            Ok(_) => None,
            Err(KosError::InvalidConfig(msg)) => {
                let cycle = self.find_cycle_dfs();
                if cycle.is_empty() {
                    Some(vec![msg])
                } else {
                    Some(cycle)
                }
            }
            Err(_) => None,
        }
    }

    pub fn impact(&self, app_id: &str) -> Vec<String> {
        let mut result = Vec::new();
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(app_id);

        let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
        for (node, deps) in &self.graph {
            for dep in deps {
                reverse
                    .entry(dep.as_str())
                    .or_default()
                    .push(node.as_str());
            }
        }

        while let Some(current) = queue.pop_front() {
            if let Some(dependants) = reverse.get(current) {
                for &dep in dependants {
                    if visited.insert(dep) {
                        result.push(dep.to_string());
                        queue.push_back(dep);
                    }
                }
            }
        }

        result.sort();
        result
    }

    pub fn dependencies_of(&self, app_id: &str) -> Vec<String> {
        self.graph
            .get(app_id)
            .cloned()
            .unwrap_or_default()
    }

    fn find_cycle_dfs(&self) -> Vec<String> {
        #[derive(Clone, Copy, PartialEq)]
        enum Color {
            White,
            Gray,
            Black,
        }

        let mut color: HashMap<&str, Color> = self
            .graph
            .keys()
            .map(|k| (k.as_str(), Color::White))
            .collect();
        let mut parent: HashMap<&str, &str> = HashMap::new();

        for start in self.graph.keys() {
            if color[start.as_str()] != Color::White {
                continue;
            }
            let mut stack = vec![start.as_str()];

            while let Some(node) = stack.last().copied() {
                match color[node] {
                    Color::White => {
                        *color.get_mut(node).unwrap() = Color::Gray;
                        if let Some(deps) = self.graph.get(node) {
                            for dep in deps {
                                match color.get(dep.as_str()) {
                                    Some(Color::Gray) => {
                                        let mut cycle = vec![dep.clone()];
                                        let mut cur = node;
                                        while cur != dep.as_str() {
                                            cycle.push(cur.to_string());
                                            cur = parent.get(cur).copied().unwrap_or(cur);
                                            if cycle.len() > self.graph.len() {
                                                break;
                                            }
                                        }
                                        cycle.reverse();
                                        return cycle;
                                    }
                                    Some(Color::White) => {
                                        parent.insert(dep.as_str(), node);
                                        stack.push(dep.as_str());
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    Color::Gray => {
                        *color.get_mut(node).unwrap() = Color::Black;
                        stack.pop();
                    }
                    Color::Black => {
                        stack.pop();
                    }
                }
            }
        }

        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, deps: &[&str]) -> AppConfig {
        AppConfig {
            id: id.into(),
            binary: id.into(),
            args: vec![],
            domain: "default".into(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            restart: Default::default(),
            schedule: Default::default(),
            priority: "normal".into(),
            params: Default::default(),
            threads: Vec::new(),
        }
    }

    #[test]
    fn linear_dependency_order() {
        let apps = vec![app("A", &[]), app("B", &["A"]), app("C", &["B"])];
        let resolver = DependencyResolver::build(&apps).unwrap();
        let order = resolver.resolve_order().unwrap();

        assert_eq!(order.len(), 3);
        assert_eq!(order[0], vec!["A"]);
        assert_eq!(order[1], vec!["B"]);
        assert_eq!(order[2], vec!["C"]);
    }

    #[test]
    fn parallel_dependency_order() {
        let apps = vec![app("A", &[]), app("B", &[]), app("C", &["A", "B"])];
        let resolver = DependencyResolver::build(&apps).unwrap();
        let order = resolver.resolve_order().unwrap();

        assert_eq!(order.len(), 2);
        assert_eq!(order[0], vec!["A", "B"]);
        assert_eq!(order[1], vec!["C"]);
    }

    #[test]
    fn circular_dependency_detected() {
        let apps = vec![app("A", &["B"]), app("B", &["A"])];
        let resolver = DependencyResolver::build(&apps).unwrap();

        assert!(resolver.resolve_order().is_err());
        assert!(resolver.check_circular().is_some());
    }

    #[test]
    fn no_dependencies() {
        let apps = vec![app("X", &[]), app("Y", &[]), app("Z", &[])];
        let resolver = DependencyResolver::build(&apps).unwrap();
        let order = resolver.resolve_order().unwrap();

        assert_eq!(order.len(), 1);
        assert_eq!(order[0], vec!["X", "Y", "Z"]);
    }

    #[test]
    fn impact_returns_downstream_apps() {
        let apps = vec![
            app("camera", &[]),
            app("lka", &["camera"]),
            app("planning", &["lka"]),
        ];
        let resolver = DependencyResolver::build(&apps).unwrap();

        let affected = resolver.impact("camera");
        assert_eq!(affected, vec!["lka", "planning"]);

        let affected = resolver.impact("lka");
        assert_eq!(affected, vec!["planning"]);

        let affected = resolver.impact("planning");
        assert!(affected.is_empty());
    }

    #[test]
    fn unknown_dependency_error() {
        let apps = vec![app("A", &["ghost"])];
        let err = DependencyResolver::build(&apps).unwrap_err();
        assert!(matches!(err, KosError::NotFound(_)));
    }
}
