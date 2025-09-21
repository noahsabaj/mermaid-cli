use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser, Query, QueryCursor};
// For tree-sitter 0.24, we need StreamingIterator
use streaming_iterator::StreamingIterator;

/// Symbol types we extract from code
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Class,
    Method,
    Interface,
    Type,
    Variable,
    Import,
    Module,
}

/// A code symbol extracted from the AST
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub file_path: PathBuf,
    pub line: usize,
    pub signature: Option<String>,
    pub doc_comment: Option<String>,
}

/// A reference to a symbol from another location
#[derive(Debug, Clone)]
pub struct SymbolReference {
    pub symbol_name: String,
    pub from_file: PathBuf,
    pub from_line: usize,
    pub to_file: Option<PathBuf>, // None if unresolved
}

/// Tree-sitter based code parser
pub struct TreeParser {
    parsers: HashMap<String, Parser>,
    queries: HashMap<String, Query>,
}

impl TreeParser {
    pub fn new() -> Result<Self> {
        let mut parsers = HashMap::new();
        let mut queries = HashMap::new();

        // Initialize Rust parser
        let mut rust_parser = Parser::new();
        rust_parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
        parsers.insert("rust".to_string(), rust_parser);

        // Rust query for extracting symbols
        let rust_query = Query::new(
            &tree_sitter_rust::LANGUAGE.into(),
            r#"
            ; Functions
            (function_item
              name: (identifier) @function.name) @function

            ; Structs
            (struct_item
              name: (type_identifier) @struct.name) @struct

            ; Enums
            (enum_item
              name: (type_identifier) @enum.name) @enum

            ; Traits
            (trait_item
              name: (type_identifier) @trait.name) @trait

            ; Impl blocks
            (impl_item
              type: (type_identifier) @impl.type) @impl

            ; Methods
            (function_item
              name: (identifier) @method.name
              parameters: (parameters
                (self_parameter))) @method

            ; Use statements
            (use_declaration
              argument: (_) @import.path) @import
            "#,
        )?;
        queries.insert("rust".to_string(), rust_query);

        // Initialize Python parser
        let mut python_parser = Parser::new();
        python_parser.set_language(&tree_sitter_python::LANGUAGE.into())?;
        parsers.insert("python".to_string(), python_parser);

        let python_query = Query::new(
            &tree_sitter_python::LANGUAGE.into(),
            r#"
            ; Functions
            (function_definition
              name: (identifier) @function.name) @function

            ; Classes
            (class_definition
              name: (identifier) @class.name) @class

            ; Methods (functions inside classes)
            (class_definition
              body: (block
                (function_definition
                  name: (identifier) @method.name))) @method

            ; Import statements
            (import_statement
              name: (dotted_name) @import.name) @import

            (import_from_statement
              module_name: (dotted_name) @import.module) @import
            "#,
        )?;
        queries.insert("python".to_string(), python_query);

        // Initialize JavaScript parser
        let mut js_parser = Parser::new();
        js_parser.set_language(&tree_sitter_javascript::LANGUAGE.into())?;
        parsers.insert("javascript".to_string(), js_parser);

        // Initialize TypeScript parser (using JavaScript parser for now)
        let mut ts_parser = Parser::new();
        ts_parser.set_language(&tree_sitter_javascript::LANGUAGE.into())?;
        parsers.insert("typescript".to_string(), ts_parser);

        let js_query_str = r#"
            ; Functions
            (function_declaration
              name: (identifier) @function.name) @function

            ; Arrow functions assigned to variables
            (variable_declarator
              name: (identifier) @function.name
              value: (arrow_function)) @function

            ; Classes
            (class_declaration
              name: (identifier) @class.name) @class

            ; Methods
            (method_definition
              name: (property_identifier) @method.name) @method

            ; Imports
            (import_statement
              source: (string) @import.source) @import
            "#;

        let js_query = Query::new(&tree_sitter_javascript::LANGUAGE.into(), js_query_str)?;
        queries.insert("javascript".to_string(), js_query);

        let ts_query = Query::new(&tree_sitter_javascript::LANGUAGE.into(), js_query_str)?;
        queries.insert("typescript".to_string(), ts_query);

        Ok(Self { parsers, queries })
    }

    /// Parse a file and extract symbols
    pub fn parse_file(&mut self, path: &Path, content: &str) -> Result<Vec<Symbol>> {
        let language = self.detect_language(path)?;

        let parser = self
            .parsers
            .get_mut(&language)
            .context(format!("No parser for language: {}", language))?;

        let tree = parser
            .parse(content, None)
            .with_context(|| format!("Failed to parse {} - file may contain syntax errors", path.display()))?;

        let query = self
            .queries
            .get(&language)
            .context(format!("No query for language: {}", language))?;

        self.extract_symbols(path, content, &tree.root_node(), query)
    }

    /// Extract symbols from parsed AST
    fn extract_symbols(
        &self,
        file_path: &Path,
        source: &str,
        root: &Node,
        query: &Query,
    ) -> Result<Vec<Symbol>> {
        let mut symbols = Vec::new();
        let mut cursor = QueryCursor::new();

        // Use StreamingIterator for tree-sitter 0.24
        let mut matches = cursor.matches(query, *root, source.as_bytes());
        while let Some(m) = matches.next() {
            for capture in m.captures {
                let node = capture.node;
                let capture_name = query.capture_names()[capture.index as usize];

                let symbol_name = node.utf8_text(source.as_bytes())?;
                let line = node.start_position().row + 1;

                let kind = match capture_name {
                    name if name.starts_with("function") => SymbolKind::Function,
                    name if name.starts_with("class") => SymbolKind::Class,
                    name if name.starts_with("method") => SymbolKind::Method,
                    name if name.starts_with("struct") => SymbolKind::Class,
                    name if name.starts_with("enum") => SymbolKind::Type,
                    name if name.starts_with("trait") => SymbolKind::Interface,
                    name if name.starts_with("import") => SymbolKind::Import,
                    _ => continue,
                };

                // Extract signature if available
                let signature = self.extract_signature(&node, source)?;

                symbols.push(Symbol {
                    name: symbol_name.to_string(),
                    kind,
                    file_path: file_path.to_path_buf(),
                    line,
                    signature,
                    doc_comment: None, // Doc comment extraction not yet implemented
                });
            }
        }

        Ok(symbols)
    }

    /// Extract function/method signature
    fn extract_signature(&self, node: &Node, source: &str) -> Result<Option<String>> {
        // For functions, try to get the full signature including parameters and return type
        if let Some(parent) = node.parent() {
            if parent.kind() == "function_item"
                || parent.kind() == "function_declaration"
                || parent.kind() == "function_definition"
            {
                // Get the line containing the function definition
                let start_byte = parent.start_byte();
                let end_byte = source[start_byte..]
                    .find('\n')
                    .map(|i| start_byte + i)
                    .unwrap_or(parent.end_byte());

                let signature = source[start_byte..end_byte].trim();
                return Ok(Some(signature.to_string()));
            }
        }
        Ok(None)
    }

    /// Find references to symbols in code
    pub fn find_references(&mut self, path: &Path, content: &str) -> Result<Vec<SymbolReference>> {
        let language = self.detect_language(path)?;

        let parser = self
            .parsers
            .get_mut(&language)
            .context(format!("No parser for language: {}", language))?;

        let tree = parser
            .parse(content, None)
            .with_context(|| format!("Failed to parse {} - file may contain syntax errors", path.display()))?;

        // This is simplified - real implementation would need language-specific
        // queries to identify symbol references vs definitions
        let references = self.extract_references(path, content, &tree.root_node())?;

        Ok(references)
    }

    /// Extract symbol references from AST
    fn extract_references(
        &self,
        file_path: &Path,
        source: &str,
        root: &Node,
    ) -> Result<Vec<SymbolReference>> {
        let mut references = Vec::new();

        // Walk the tree looking for identifiers that are references, not definitions
        let mut cursor = root.walk();
        self.visit_node_for_references(&mut cursor, file_path, source, &mut references)?;

        Ok(references)
    }

    /// Recursively visit nodes looking for references
    fn visit_node_for_references(
        &self,
        cursor: &mut tree_sitter::TreeCursor,
        file_path: &Path,
        source: &str,
        references: &mut Vec<SymbolReference>,
    ) -> Result<()> {
        let node = cursor.node();

        // Check if this node is a reference (simplified logic)
        if node.kind() == "identifier" || node.kind() == "type_identifier" {
            // Skip if this is a definition (has certain parent types)
            if let Some(parent) = node.parent() {
                let parent_kind = parent.kind();
                if parent_kind.contains("definition")
                    || parent_kind.contains("declaration")
                    || parent_kind.contains("parameter")
                {
                    // This is a definition, not a reference
                } else {
                    // This looks like a reference
                    let symbol_name = node.utf8_text(source.as_bytes())?;
                    references.push(SymbolReference {
                        symbol_name: symbol_name.to_string(),
                        from_file: file_path.to_path_buf(),
                        from_line: node.start_position().row + 1,
                        to_file: None, // Would need symbol resolution to determine
                    });
                }
            }
        }

        // Recurse to children
        if cursor.goto_first_child() {
            loop {
                self.visit_node_for_references(cursor, file_path, source, references)?;
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
            cursor.goto_parent();
        }

        Ok(())
    }

    /// Detect language from file extension
    fn detect_language(&self, path: &Path) -> Result<String> {
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .context("No file extension")?;

        let language = match extension {
            "rs" => "rust",
            "py" => "python",
            "js" | "mjs" => "javascript",
            "ts" | "tsx" => "typescript",
            "go" => "go",
            "java" => "java",
            "cpp" | "cc" | "cxx" => "cpp",
            _ => return Err(anyhow::anyhow!("Unsupported file type: {}", extension)),
        };

        Ok(language.to_string())
    }

    /// Get all supported languages
    pub fn supported_extensions(&self) -> HashSet<&str> {
        let mut extensions = HashSet::new();
        extensions.insert("rs");
        extensions.insert("py");
        extensions.insert("js");
        extensions.insert("mjs");
        extensions.insert("ts");
        extensions.insert("tsx");
        extensions.insert("go");
        extensions.insert("java");
        extensions.insert("cpp");
        extensions.insert("cc");
        extensions.insert("cxx");
        extensions
    }
}