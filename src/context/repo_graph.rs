use anyhow::Result;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::tree_parser::{Symbol, SymbolKind, SymbolReference};

/// Node in the repository graph representing a file
#[derive(Debug, Clone)]
pub struct FileNode {
    pub path: PathBuf,
    pub symbols: Vec<Symbol>,
    pub importance_score: f64,
}

/// Edge in the repository graph representing a dependency
#[derive(Debug, Clone)]
pub struct DependencyEdge {
    pub weight: f64, // How many times this symbol is referenced
}

/// Repository dependency graph
pub struct RepoGraph {
    graph: DiGraph<FileNode, DependencyEdge>,
    file_indices: HashMap<PathBuf, NodeIndex>,
    symbol_locations: HashMap<String, Vec<PathBuf>>, // Symbol name -> files defining it
}

impl RepoGraph {
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            file_indices: HashMap::new(),
            symbol_locations: HashMap::new(),
        }
    }

    /// Add a file and its symbols to the graph
    pub fn add_file(&mut self, path: PathBuf, symbols: Vec<Symbol>) -> NodeIndex {
        // Check if file already exists
        if let Some(&index) = self.file_indices.get(&path) {
            return index;
        }

        // Track symbol definitions
        for symbol in &symbols {
            self.symbol_locations
                .entry(symbol.name.clone())
                .or_insert_with(Vec::new)
                .push(path.clone());
        }

        // Create file node
        let node = FileNode {
            path: path.clone(),
            symbols,
            importance_score: 1.0, // Initial score
        };

        let index = self.graph.add_node(node);
        self.file_indices.insert(path, index);
        index
    }

    /// Add references between files based on symbol usage
    pub fn add_references(&mut self, references: Vec<SymbolReference>) {
        // Group references by source file
        let mut references_by_file: HashMap<PathBuf, Vec<SymbolReference>> = HashMap::new();
        for reference in references {
            references_by_file
                .entry(reference.from_file.clone())
                .or_insert_with(Vec::new)
                .push(reference);
        }

        // Process references for each file
        for (from_file, refs) in references_by_file {
            let from_index = match self.file_indices.get(&from_file) {
                Some(&idx) => idx,
                None => continue, // File not in graph
            };

            // Count references to each target file
            let mut reference_counts: HashMap<PathBuf, HashMap<String, usize>> = HashMap::new();

            for reference in refs {
                // Try to resolve where this symbol is defined
                if let Some(defining_files) = self.symbol_locations.get(&reference.symbol_name) {
                    for defining_file in defining_files {
                        if defining_file != &from_file {
                            // Don't create self-edges
                            reference_counts
                                .entry(defining_file.clone())
                                .or_insert_with(HashMap::new)
                                .entry(reference.symbol_name.clone())
                                .and_modify(|c| *c += 1)
                                .or_insert(1);
                        }
                    }
                }
            }

            // Create edges for references
            for (to_file, symbol_counts) in reference_counts {
                if let Some(&to_index) = self.file_indices.get(&to_file) {
                    // Check if edge already exists
                    let existing_edge = self
                        .graph
                        .edges_connecting(from_index, to_index)
                        .next()
                        .map(|e| e.id());

                    if let Some(edge_id) = existing_edge {
                        // Update existing edge weight
                        if let Some(edge_weight) = self.graph.edge_weight_mut(edge_id) {
                            for (_symbol, count) in symbol_counts {
                                edge_weight.weight += count as f64;
                                // Could track individual symbols if needed
                            }
                        }
                    } else {
                        // Create new edge
                        let total_weight: usize = symbol_counts.values().sum();
                        let edge = DependencyEdge {
                            weight: total_weight as f64,
                        };
                        self.graph.add_edge(from_index, to_index, edge);
                    }
                }
            }
        }
    }

    /// Run PageRank algorithm to compute file importance
    pub fn compute_pagerank(
        &mut self,
        damping: f64,
        iterations: usize,
        personalization: Option<HashMap<PathBuf, f64>>,
    ) -> Result<()> {
        let num_nodes = self.graph.node_count();
        if num_nodes == 0 {
            return Ok(());
        }

        // Initialize scores
        let initial_score = 1.0 / num_nodes as f64;
        let mut scores: Vec<f64> = vec![initial_score; num_nodes];
        let mut new_scores = vec![0.0; num_nodes];

        // Create index mapping for efficient access
        let index_to_position: HashMap<NodeIndex, usize> = self
            .graph
            .node_indices()
            .enumerate()
            .map(|(pos, idx)| (idx, pos))
            .collect();

        // Apply personalization if provided
        if let Some(ref personalization) = personalization {
            for (path, weight) in personalization {
                if let Some(&node_idx) = self.file_indices.get(path) {
                    if let Some(&pos) = index_to_position.get(&node_idx) {
                        scores[pos] = *weight;
                    }
                }
            }

            // Normalize scores
            let sum: f64 = scores.iter().sum();
            if sum > 0.0 {
                for score in &mut scores {
                    *score /= sum;
                }
            }
        }

        // PageRank iterations
        for _ in 0..iterations {
            // Reset new scores
            new_scores.fill((1.0 - damping) / num_nodes as f64);

            // Distribute scores through edges
            for node_idx in self.graph.node_indices() {
                let pos = index_to_position[&node_idx];
                let score = scores[pos];

                // Get outgoing edges
                let out_edges: Vec<_> = self.graph.edges(node_idx).collect();
                if !out_edges.is_empty() {
                    let total_weight: f64 = out_edges.iter().map(|e| e.weight().weight).sum();

                    for edge in out_edges {
                        let target = edge.target();
                        let target_pos = index_to_position[&target];
                        let edge_weight = edge.weight().weight;

                        // Transfer score proportional to edge weight
                        new_scores[target_pos] += damping * score * (edge_weight / total_weight);
                    }
                } else {
                    // No outgoing edges - distribute equally (handling dangling nodes)
                    let share = damping * score / num_nodes as f64;
                    for score in &mut new_scores {
                        *score += share;
                    }
                }
            }

            // Apply personalization in each iteration
            if let Some(ref personalization) = personalization {
                for (path, weight) in personalization {
                    if let Some(&node_idx) = self.file_indices.get(path) {
                        if let Some(&pos) = index_to_position.get(&node_idx) {
                            new_scores[pos] += (1.0 - damping) * weight;
                        }
                    }
                }
            }

            // Swap score vectors
            std::mem::swap(&mut scores, &mut new_scores);
        }

        // Update node importance scores
        for node_idx in self.graph.node_indices() {
            if let Some(&pos) = index_to_position.get(&node_idx) {
                if let Some(node) = self.graph.node_weight_mut(node_idx) {
                    node.importance_score = scores[pos];
                }
            }
        }

        Ok(())
    }

    /// Get files ranked by importance
    pub fn get_ranked_files(&self) -> Vec<(&Path, f64)> {
        let mut files: Vec<_> = self
            .graph
            .node_weights()
            .map(|node| (node.path.as_path(), node.importance_score))
            .collect();

        files.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        files
    }

    /// Get ranked symbols across all files
    pub fn get_ranked_symbols(&self, limit: Option<usize>) -> Vec<RankedSymbol> {
        let mut all_symbols = Vec::new();

        for node in self.graph.node_weights() {
            for symbol in &node.symbols {
                // Score symbols based on file importance and symbol type
                let type_weight = match symbol.kind {
                    SymbolKind::Class | SymbolKind::Interface => 1.5,
                    SymbolKind::Function | SymbolKind::Method => 1.2,
                    SymbolKind::Type => 1.1,
                    _ => 1.0,
                };

                let symbol_score = node.importance_score * type_weight;

                all_symbols.push(RankedSymbol {
                    symbol: symbol.clone(),
                    score: symbol_score,
                });
            }
        }

        // Sort by score
        all_symbols.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        // Apply limit if specified
        if let Some(limit) = limit {
            all_symbols.truncate(limit);
        }

        all_symbols
    }

    /// Get symbols for specific files
    pub fn get_file_symbols(&self, path: &Path) -> Option<Vec<Symbol>> {
        self.file_indices.get(path).and_then(|&idx| {
            self.graph
                .node_weight(idx)
                .map(|node| node.symbols.clone())
        })
    }

    /// Get graph statistics
    pub fn stats(&self) -> GraphStats {
        GraphStats {
            total_files: self.graph.node_count(),
            total_edges: self.graph.edge_count(),
            total_symbols: self
                .graph
                .node_weights()
                .map(|n| n.symbols.len())
                .sum(),
            unique_symbols: self.symbol_locations.len(),
        }
    }

    /// Clear the graph
    pub fn clear(&mut self) {
        self.graph.clear();
        self.file_indices.clear();
        self.symbol_locations.clear();
    }
}

/// A symbol with its importance score
#[derive(Debug, Clone)]
pub struct RankedSymbol {
    pub symbol: Symbol,
    pub score: f64,
}

/// Graph statistics
#[derive(Debug)]
pub struct GraphStats {
    pub total_files: usize,
    pub total_edges: usize,
    pub total_symbols: usize,
    pub unique_symbols: usize,
}

/// Create personalization vector for PageRank
/// Higher weights for files in active context
pub fn create_personalization(
    chat_files: &[PathBuf],
    mentioned_files: &[PathBuf],
    other_files: &[PathBuf],
) -> HashMap<PathBuf, f64> {
    let mut personalization = HashMap::new();

    // Chat files get highest weight
    for file in chat_files {
        personalization.insert(file.clone(), 10.0);
    }

    // Mentioned files get medium weight
    for file in mentioned_files {
        personalization.insert(file.clone(), 5.0);
    }

    // Other files get base weight
    for file in other_files {
        personalization.entry(file.clone()).or_insert(1.0);
    }

    // Normalize weights
    let total: f64 = personalization.values().sum();
    if total > 0.0 {
        for weight in personalization.values_mut() {
            *weight /= total;
        }
    }

    personalization
}