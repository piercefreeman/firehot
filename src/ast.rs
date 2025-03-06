use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
};
use walkdir::WalkDir;

use rustpython_parser::ast::{
    Mod, Stmt, StmtAsyncFunctionDef, StmtClassDef, StmtFunctionDef, StmtIf, StmtWhile,
};
use rustpython_parser::{parse, Mode};

// Add PyO3 imports for Python bindings
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::sync::{Arc, Mutex};
use uuid::Uuid;
use sha2::{Sha256, Digest};

/// A simple structure to hold information about an import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportInfo {
    /// For an `import X`, this is "X". For a `from X import Y`, this is "X".
    pub module: String,
    /// The names imported from that module.
    pub names: Vec<String>,
    /// Whether this is a relative import (starts with . or ..)
    pub is_relative: bool,
}

/// A structure to manage AST parsing and import tracking for a project
pub struct ProjectAstManager {
    /// Mapping of file paths to their content SHA256 hash
    file_hashes: HashMap<String, String>,
    /// Mapping of file paths to their imports
    file_imports: HashMap<String, Vec<ImportInfo>>,
    /// The detected package name, if any
    package_name: Option<String>,
    /// The root path of the project
    project_path: String,
}

impl ProjectAstManager {
    /// Create a new ProjectAstManager for the given project path
    pub fn new(project_path: &str) -> Self {
        Self {
            file_hashes: HashMap::new(),
            file_imports: HashMap::new(),
            package_name: None,
            project_path: project_path.to_string(),
        }
    }

    /// Calculate SHA256 hash of file content
    fn calculate_file_hash(&self, file_path: &str) -> Result<String> {
        let content = fs::read(file_path)?;
        let mut hasher = Sha256::new();
        hasher.update(&content);
        let hash = hasher.finalize();
        Ok(format!("{:x}", hash))
    }

    /// Detect the package name for the project
    pub fn detect_package_name(&mut self) -> Result<Option<String>> {
        let path = Path::new(&self.project_path);
        self.package_name = detect_package_name(path);
        Ok(self.package_name.clone())
    }

    /// Get the project path
    pub fn get_project_path(&self) -> &str {
        &self.project_path
    }

    /// Process a single Python file and extract its imports
    fn process_py_file(&mut self, file_path: &str) -> Result<Vec<ImportInfo>> {
        // Calculate hash of the file content
        let new_hash = self.calculate_file_hash(file_path)?;
        
        // Check if we have already processed this file and if the content has changed
        if let Some(old_hash) = self.file_hashes.get(file_path) {
            if old_hash == &new_hash {
                // File hasn't changed, return cached imports
                return Ok(self.file_imports.get(file_path).cloned().unwrap_or_default());
            }
        }
        
        // File is new or has changed, parse it
        let source = fs::read_to_string(file_path)?;
        let parsed = parse(&source, Mode::Module, file_path)
            .map_err(|e| anyhow!("Failed to parse {}: {:?}", file_path, e))?;
        
        // Extract statements from the module
        let stmts: &[Stmt] = match &parsed {
            Mod::Module(module) => &module.body,
            _ => {
                return Err(anyhow!(
                    "Unexpected AST format for module in file {}",
                    file_path
                ))
            }
        };
        
        // Collect imports
        let imports = collect_imports(stmts);
        
        // Update caches
        self.file_hashes.insert(file_path.to_string(), new_hash);
        self.file_imports.insert(file_path.to_string(), imports.clone());
        
        Ok(imports)
    }

    /// Process all Python files in the project and extract their imports
    pub fn process_all_py_files(&mut self) -> Result<HashSet<String>> {
        let mut third_party_modules = HashSet::new();
        
        // Ensure we have detected the package name
        if self.package_name.is_none() {
            self.detect_package_name()?;
        }

        let path = Path::new(&self.project_path);
        let package_path = path.join(self.package_name.clone().unwrap_or_default());

        for entry in WalkDir::new(package_path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_type().is_file() && e.path().extension().map(|ext| ext == "py").unwrap_or(false)
            })
        {
            let file_path = entry.path().to_string_lossy().to_string();
            let imports = self.process_py_file(&file_path)?;
            
            for imp in imports {
                // Skip relative imports and imports of the current package
                if !imp.is_relative
                    && !self.package_name
                        .as_ref()
                        .map_or(false, |pkg| imp.module.starts_with(pkg))
                {
                    third_party_modules.insert(imp.module);
                }
            }
        }
        
        Ok(third_party_modules)
    }

    /// Compute the delta of imports between the current state and the previous state
    pub fn compute_import_delta(&mut self) -> Result<(HashSet<String>, HashSet<String>)> {
        // Store previous imports
        let previous_imports: HashSet<String> = self
            .file_imports
            .values()
            .flatten()
            .filter(|imp| {
                !imp.is_relative
                    && !self.package_name
                        .as_ref()
                        .map_or(false, |pkg| imp.module.starts_with(pkg))
            })
            .map(|imp| imp.module.clone())
            .collect();
        
        // Get current imports
        let current_imports = self.process_all_py_files()?;
        
        // Calculate added and removed imports
        let added: HashSet<String> = current_imports
            .difference(&previous_imports)
            .cloned()
            .collect();
        
        let removed: HashSet<String> = previous_imports
            .difference(&current_imports)
            .cloned()
            .collect();
        
        Ok((added, removed))
    }

    /// Get the package name
    pub fn get_package_name(&self) -> Option<String> {
        self.package_name.clone()
    }
}

/// Recursively traverse AST statements to collect import information.
/// Absolute (level == 0) imports are considered third-party.
pub fn collect_imports(stmts: &[Stmt]) -> Vec<ImportInfo> {
    let mut imports = Vec::new();
    for stmt in stmts {
        match stmt {
            Stmt::Import(import_stmt) => {
                for alias in &import_stmt.names {
                    imports.push(ImportInfo {
                        module: alias.name.to_string(),
                        names: vec![alias
                            .asname
                            .clone()
                            .unwrap_or_else(|| alias.name.clone())
                            .to_string()],
                        is_relative: false,
                    });
                }
            }
            Stmt::ImportFrom(import_from) => {
                if let Some(module_name) = &import_from.module {
                    let imported = import_from
                        .names
                        .iter()
                        .map(|alias| {
                            alias
                                .asname
                                .clone()
                                .unwrap_or_else(|| alias.name.clone())
                                .to_string()
                        })
                        .collect();
                    imports.push(ImportInfo {
                        module: module_name.to_string(),
                        names: imported,
                        is_relative: import_from.level.map_or(false, |level| level.to_u32() > 0),
                    });
                }
            }
            Stmt::If(inner) => {
                let if_stmt: &StmtIf = inner;
                imports.extend(collect_imports(&if_stmt.body));
                imports.extend(collect_imports(&if_stmt.orelse));
            }
            Stmt::While(inner) => {
                let while_stmt: &StmtWhile = inner;
                imports.extend(collect_imports(&while_stmt.body));
                imports.extend(collect_imports(&while_stmt.orelse));
            }
            Stmt::FunctionDef(inner) => {
                let func_def: &StmtFunctionDef = inner;
                imports.extend(collect_imports(&func_def.body));
            }
            Stmt::AsyncFunctionDef(inner) => {
                let func_def: &StmtAsyncFunctionDef = inner;
                imports.extend(collect_imports(&func_def.body));
            }
            Stmt::ClassDef(inner) => {
                let class_def: &StmtClassDef = inner;
                imports.extend(collect_imports(&class_def.body));
            }
            _ => {}
        }
    }
    imports
}

/// Detect the current package name by looking for setup.py, pyproject.toml, or top-level __init__.py files
fn detect_package_name(path: &Path) -> Option<String> {
    // Try to find setup.py
    for entry in WalkDir::new(path)
        .max_depth(2) // Only check top-level and immediate subdirectories
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let file_path = entry.path();
        if file_path.file_name().unwrap_or_default() == "setup.py" {
            if let Ok(content) = fs::read_to_string(file_path) {
                // Look for name='package_name' or name="package_name"
                let name_re = regex::Regex::new(r#"name=["']([^"']+)["']"#).unwrap();
                if let Some(captures) = name_re.captures(&content) {
                    return Some(captures.get(1).unwrap().as_str().to_string());
                }
            }
        } else if file_path.file_name().unwrap_or_default() == "pyproject.toml" {
            if let Ok(content) = fs::read_to_string(file_path) {
                // Look for name = "package_name" in [project] or [tool.poetry] section
                let name_re = regex::Regex::new(
                    r#"(?:\[project\]|\[tool\.poetry\]).*?name\s*=\s*["']([^"']+)["']"#,
                )
                .unwrap();
                if let Some(captures) = name_re.captures(&content) {
                    return Some(captures.get(1).unwrap().as_str().to_string());
                }
            }
        }
    }

    // If no setup.py or pyproject.toml found, use directory name as fallback
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|s| s.to_string())
}

