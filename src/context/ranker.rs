use anyhow::Result;
use std::path::PathBuf;
use tiktoken_rs::{cl100k_base, CoreBPE};

use super::repo_graph::{RankedSymbol, RepoGraph};
use super::tree_parser::{Symbol, SymbolKind};

/// Configuration for the ranking system
#[derive(Debug, Clone)]
pub struct RankerConfig {
    pub max_tokens: usize,
    pub damping_factor: f64,
    pub pagerank_iterations: usize,
    pub include_signatures: bool,
    pub include_doc_comments: bool,
}

impl Default for RankerConfig {
    fn default() -> Self {
        Self {
            max_tokens: 1024,        // Default token budget for repo map
            damping_factor: 0.85,    // Standard PageRank damping
            pagerank_iterations: 30, // Usually converges within 20-30 iterations
            include_signatures: true,
            include_doc_comments: false, // Often too verbose for initial map
        }
    }
}

/// Repository ranker using PageRank and token optimization
pub struct RepoRanker {
    config: RankerConfig,
    tokenizer: CoreBPE,
    graph: RepoGraph,
}

impl RepoRanker {
    pub fn new(config: RankerConfig) -> Result<Self> {
        let tokenizer = cl100k_base()?;
        Ok(Self {
            config,
            tokenizer,
            graph: RepoGraph::new(),
        })
    }

    /// Get mutable reference to the graph
    pub fn graph_mut(&mut self) -> &mut RepoGraph {
        &mut self.graph
    }

    /// Run PageRank with personalization
    pub fn rank_with_context(
        &mut self,
        chat_files: &[PathBuf],
        mentioned_files: &[PathBuf],
    ) -> Result<()> {
        // Create personalization vector
        let personalization = super::repo_graph::create_personalization(
            chat_files,
            mentioned_files,
            &[], // Other files handled internally
        );

        // Run PageRank
        self.graph.compute_pagerank(
            self.config.damping_factor,
            self.config.pagerank_iterations,
            Some(personalization),
        )?;

        Ok(())
    }

    /// Find optimal symbol set using binary search to fit token budget
    pub fn optimize_symbols(&self, token_budget: usize) -> Result<Vec<RankedSymbol>> {
        // Get all ranked symbols
        let all_symbols = self.graph.get_ranked_symbols(None);

        if all_symbols.is_empty() {
            return Ok(Vec::new());
        }

        // Binary search for the maximum number of symbols that fit
        let mut left = 0;
        let mut right = all_symbols.len();
        let mut best_fit = 0;

        while left <= right {
            let mid = (left + right) / 2;
            let symbols = &all_symbols[0..mid];

            let token_count = self.estimate_token_count(symbols)?;

            if token_count <= token_budget {
                best_fit = mid;
                left = mid + 1;
            } else {
                if mid == 0 {
                    break;
                }
                right = mid - 1;
            }
        }

        Ok(all_symbols[0..best_fit].to_vec())
    }

    /// Estimate token count for a set of symbols
    pub fn estimate_token_count(&self, symbols: &[RankedSymbol]) -> Result<usize> {
        let mut total_tokens = 0;

        for ranked_symbol in symbols {
            // Count tokens in symbol representation
            total_tokens += self.estimate_symbol_tokens(&ranked_symbol.symbol)?;
        }

        Ok(total_tokens)
    }

    /// Estimate tokens for a single symbol
    fn estimate_symbol_tokens(&self, symbol: &Symbol) -> Result<usize> {
        let mut text = String::new();

        // File path (shortened)
        if let Some(file_name) = symbol.file_path.file_name() {
            text.push_str(&format!("{}:", file_name.to_string_lossy()));
        }

        // Line number
        text.push_str(&format!("{} ", symbol.line));

        // Symbol kind and name
        text.push_str(&format!("{:?} {}", symbol.kind, symbol.name));

        // Include signature if configured
        if self.config.include_signatures {
            if let Some(ref signature) = symbol.signature {
                text.push_str(&format!(" {}", signature));
            }
        }

        // Include doc comment if configured
        if self.config.include_doc_comments {
            if let Some(ref doc) = symbol.doc_comment {
                text.push_str(&format!(" // {}", doc));
            }
        }

        text.push('\n');

        // Count tokens
        let tokens = self.tokenizer.encode_with_special_tokens(&text);
        Ok(tokens.len())
    }

    /// Generate repository map optimized for token budget
    pub fn generate_map(&self, token_budget: Option<usize>) -> Result<String> {
        let budget = token_budget.unwrap_or(self.config.max_tokens);
        let symbols = self.optimize_symbols(budget)?;

        self.format_repository_map(&symbols)
    }

    /// Format symbols into a readable repository map
    fn format_repository_map(&self, symbols: &[RankedSymbol]) -> Result<String> {
        let mut map = String::new();
        let mut current_file = PathBuf::new();

        map.push_str("Repository Map\n");
        map.push_str("=" . repeat(50).as_str());
        map.push_str("\n\n");

        for ranked_symbol in symbols {
            let symbol = &ranked_symbol.symbol;

            // Group by file
            if symbol.file_path != current_file {
                current_file = symbol.file_path.clone();
                map.push_str(&format!("\n{}\n", current_file.display()));
                map.push_str(&"-".repeat(current_file.to_string_lossy().len()));
                map.push('\n');
            }

            // Format symbol entry
            map.push_str(&self.format_symbol_entry(symbol)?);
        }

        Ok(map)
    }

    /// Format a single symbol entry
    fn format_symbol_entry(&self, symbol: &Symbol) -> Result<String> {
        let mut entry = String::new();

        // Indentation based on symbol type
        let indent = match symbol.kind {
            SymbolKind::Method => "    ",
            _ => "  ",
        };

        entry.push_str(indent);

        // Symbol kind icon
        let icon = match symbol.kind {
            SymbolKind::Function => "ƒ",
            SymbolKind::Class => "◆",
            SymbolKind::Method => "->",
            SymbolKind::Interface => "◇",
            SymbolKind::Type => "τ",
            SymbolKind::Variable => "v",
            SymbolKind::Import => "v",
            SymbolKind::Module => "□",
        };

        entry.push_str(&format!("{} ", icon));

        // Line number and name
        entry.push_str(&format!("L{}: {}", symbol.line, symbol.name));

        // Add signature if available and configured
        if self.config.include_signatures {
            if let Some(ref signature) = symbol.signature {
                // Truncate long signatures
                let sig = if signature.len() > 60 {
                    format!("{}...", &signature[..57])
                } else {
                    signature.clone()
                };
                entry.push_str(&format!("\n{}  {}", indent, sig));
            }
        }

        entry.push('\n');
        Ok(entry)
    }

    /// Get statistics about the current ranking
    pub fn get_stats(&self) -> RankingStats {
        let all_symbols = self.graph.get_ranked_symbols(None);
        let optimal_symbols = self
            .optimize_symbols(self.config.max_tokens)
            .unwrap_or_default();

        RankingStats {
            total_symbols: all_symbols.len(),
            selected_symbols: optimal_symbols.len(),
            estimated_tokens: self
                .estimate_token_count(&optimal_symbols)
                .unwrap_or(0),
            max_token_budget: self.config.max_tokens,
        }
    }
}

/// Statistics about the ranking process
#[derive(Debug)]
pub struct RankingStats {
    pub total_symbols: usize,
    pub selected_symbols: usize,
    pub estimated_tokens: usize,
    pub max_token_budget: usize,
}

