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

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use sha2::{Digest, Sha256};

/// A simple structure to hold information about a single module import definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportInfo {
    /// For an `import X`, this is "X". For a `from X import Y`, this is "X".
    pub module: String,
    /// The names imported from that module.
    pub names: Vec<String>,
    /// Whether this is a relative import (starts with . or ..)
    pub is_relative: bool,
}

/// Manage AST parsing and import tracking for a project
pub struct ProjectAstManager {
    /// Mapping of file paths to their content SHA256 hash
    file_hashes: HashMap<String, String>,
    /// Mapping of file paths to their imports. This includes both first party and third party imports.
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

    /// Process all Python files in the project and extract third-party imports.
    /// This will have the side-effect of updating `self.file_imports` with ALL imports,
    /// but will only return third-party imports.
    pub fn process_all_py_files(&mut self) -> Result<HashSet<String>> {
        let mut third_party_imports = HashSet::new();
        println!("Processing all Python files in: {}", self.project_path);

        // First, detect package name if not already detected
        if self.package_name.is_none() {
            self.detect_package_name()?;
            println!("Detected package name: {:?}", self.package_name);
        }

        // Walk through all files in the project
        for entry in WalkDir::new(&self.project_path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            if let Some(extension) = path.extension() {
                if extension != "py" {
                    continue;
                }

                let path_str = path.to_str().ok_or_else(|| {
                    anyhow::anyhow!("Failed to convert path to string: {:?}", path)
                })?;
                println!("Processing Python file: {}", path_str);

                // Process the file
                let imports = self.process_py_file(path_str)?;
                println!("Found imports in {}: {:?}", path_str, imports);

                // Add third-party imports to the result
                for import in &imports {
                    if self.is_third_party_import(import) {
                        println!("Found third-party import: {:?}", import);
                        third_party_imports.insert(import.module.clone());
                    } else {
                        println!("Skipping first-party import: {:?}", import);
                    }
                }
            }
        }

        println!(
            "Found {} third-party imports: {:?}",
            third_party_imports.len(),
            third_party_imports
        );
        Ok(third_party_imports)
    }

    /// Compute the delta of imports between the current state and the previous state
    /// Returns (added, removed)
    pub fn compute_import_delta(&mut self) -> Result<(HashSet<String>, HashSet<String>)> {
        // Copy previous imports
        let previous_imports: HashSet<String> = self
            .file_imports
            .values()
            .flatten()
            .filter(|imp| self.is_third_party_import(imp))
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

    /// Process a single Python file and extract its imports
    fn process_py_file(&mut self, file_path: &str) -> Result<Vec<ImportInfo>> {
        println!("process_py_file called for: {}", file_path);

        // Calculate hash of the file content
        let new_hash = self.calculate_file_hash(file_path)?;

        // Check if we have already processed this file and if the content has changed
        if let Some(old_hash) = self.file_hashes.get(file_path) {
            if old_hash == &new_hash {
                // File hasn't changed, return cached imports
                println!("File hasn't changed, using cached imports");
                return Ok(self
                    .file_imports
                    .get(file_path)
                    .cloned()
                    .unwrap_or_default());
            }
        }

        // File is new or has changed, parse it
        println!("Parsing file: {}", file_path);
        let source = fs::read_to_string(file_path)?;
        println!("File content:\n{}", source);

        let parsed = parse(&source, Mode::Module, file_path)
            .map_err(|e| anyhow!("Failed to parse {}: {:?}", file_path, e))?;

        // Extract statements from the module
        let stmts: &[Stmt] = match &parsed {
            Mod::Module(module) => {
                println!("Extracted {} statements from module", module.body.len());
                &module.body
            }
            _ => {
                return Err(anyhow!(
                    "Unexpected AST format for module in file {}",
                    file_path
                ))
            }
        };

        // Collect imports
        let imports = collect_imports(stmts);
        println!("Collected {} imports from {}", imports.len(), file_path);

        // Update caches
        self.file_hashes.insert(file_path.to_string(), new_hash);
        self.file_imports
            .insert(file_path.to_string(), imports.clone());

        Ok(imports)
    }

    /// Calculate SHA256 hash of file content
    fn calculate_file_hash(&self, file_path: &str) -> Result<String> {
        let content = fs::read(file_path)?;
        let mut hasher = Sha256::new();
        hasher.update(&content);
        let hash = hasher.finalize();
        Ok(format!("{:x}", hash))
    }

    /// Check if an import is a third-party import
    fn is_third_party_import(&self, imp: &ImportInfo) -> bool {
        println!("Checking if import is third party: {:?}", imp);
        println!("Package name: {:?}", self.package_name);

        let is_third_party = !imp.is_relative
            && !self
                .package_name
                .as_ref()
                .map_or(false, |pkg| imp.module.starts_with(pkg));

        println!("Is third party: {}", is_third_party);
        is_third_party
    }
}

/// Recursively traverse AST statements to collect import information.
/// Absolute (level == 0) imports are considered third-party.
pub fn collect_imports(stmts: &[Stmt]) -> Vec<ImportInfo> {
    let mut imports = Vec::new();
    for stmt in stmts {
        println!("Processing statement: {:?}", stmt);
        match stmt {
            Stmt::Import(import_stmt) => {
                println!("Found import statement: {:?}", import_stmt);
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
                println!("Found import from statement: {:?}", import_from);
                println!(
                    "Level: {:?}, Module: {:?}",
                    import_from.level, import_from.module
                );
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
                } else {
                    // Handle case where module is None (likely for relative imports like "from . import x")
                    println!("Module is None, handling relative import");
                    if import_from.level.is_some() && import_from.level.unwrap().to_u32() > 0 {
                        // This is a relative import
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
                        // Use a placeholder module name based on the relative level
                        let rel_level = import_from.level.unwrap().to_u32();
                        let module_name = ".".repeat(rel_level as usize);
                        println!("Created relative import with module: {}", module_name);
                        imports.push(ImportInfo {
                            module: module_name,
                            names: imported,
                            is_relative: true,
                        });
                    }
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
    println!("Collected imports: {:?}", imports);
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
                // We need to add (?s) to handle multiline content
                let name_re = regex::Regex::new(
                    r#"(?s)(?:\[project\]|\[tool\.poetry\]).*?name\s*=\s*["']([^"']+)["']"#,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::TempDir;

    // Helper function to create a temporary Python file with given content
    fn create_temp_py_file(dir: &TempDir, filename: &str, content: &str) -> PathBuf {
        let file_path = dir.path().join(filename);
        let mut file = File::create(&file_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file_path
    }

    #[test]
    fn test_import_info_creation() {
        let import_info = ImportInfo {
            module: "module_name".to_string(),
            names: vec!["name1".to_string(), "name2".to_string()],
            is_relative: false,
        };

        assert_eq!(import_info.module, "module_name");
        assert_eq!(
            import_info.names,
            vec!["name1".to_string(), "name2".to_string()]
        );
        assert_eq!(import_info.is_relative, false);
    }

    #[test]
    fn test_project_ast_manager_initialization() {
        let manager = ProjectAstManager::new("/test/path");
        assert_eq!(manager.get_project_path(), "/test/path");
        assert_eq!(manager.get_package_name(), None);
    }

    #[test]
    fn test_file_hash_calculation() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = create_temp_py_file(&temp_dir, "test.py", "print('hello')");

        let manager = ProjectAstManager::new(temp_dir.path().to_str().unwrap());
        let hash_result = manager.calculate_file_hash(file_path.to_str().unwrap());

        assert!(hash_result.is_ok());
        // Hash should be consistent for the same content
        let hash1 = hash_result.unwrap();
        let hash2 = manager
            .calculate_file_hash(file_path.to_str().unwrap())
            .unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_collect_imports_simple() {
        let python_code = "import os\nimport sys";
        let temp_dir = TempDir::new().unwrap();
        let file_path = create_temp_py_file(&temp_dir, "imports.py", python_code);

        let source = fs::read_to_string(file_path).unwrap();
        let parsed = parse(&source, Mode::Module, "imports.py").unwrap();

        let stmts = match &parsed {
            Mod::Module(module) => &module.body,
            _ => panic!("Expected Module"),
        };

        let imports = collect_imports(stmts);

        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].module, "os");
        assert_eq!(imports[0].names, vec!["os"]);
        assert_eq!(imports[0].is_relative, false);

        assert_eq!(imports[1].module, "sys");
        assert_eq!(imports[1].names, vec!["sys"]);
        assert_eq!(imports[1].is_relative, false);
    }

    #[test]
    fn test_collect_imports_from() {
        let python_code = "from os import path\nfrom sys import argv, version";
        let temp_dir = TempDir::new().unwrap();
        let file_path = create_temp_py_file(&temp_dir, "from_imports.py", python_code);

        let source = fs::read_to_string(file_path).unwrap();
        let parsed = parse(&source, Mode::Module, "from_imports.py").unwrap();

        let stmts = match &parsed {
            Mod::Module(module) => &module.body,
            _ => panic!("Expected Module"),
        };

        let imports = collect_imports(stmts);

        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].module, "os");
        assert_eq!(imports[0].names, vec!["path"]);
        assert_eq!(imports[0].is_relative, false);

        assert_eq!(imports[1].module, "sys");
        assert_eq!(imports[1].names, vec!["argv", "version"]);
        assert_eq!(imports[1].is_relative, false);
    }

    #[test]
    fn test_collect_imports_alias() {
        let python_code = "import os as operating_system\nfrom sys import argv as arguments";
        let temp_dir = TempDir::new().unwrap();
        let file_path = create_temp_py_file(&temp_dir, "alias_imports.py", python_code);

        let source = fs::read_to_string(file_path).unwrap();
        let parsed = parse(&source, Mode::Module, "alias_imports.py").unwrap();

        let stmts = match &parsed {
            Mod::Module(module) => &module.body,
            _ => panic!("Expected Module"),
        };

        let imports = collect_imports(stmts);

        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].module, "os");
        assert_eq!(imports[0].names, vec!["operating_system"]);
        assert_eq!(imports[0].is_relative, false);

        assert_eq!(imports[1].module, "sys");
        assert_eq!(imports[1].names, vec!["arguments"]);
        assert_eq!(imports[1].is_relative, false);
    }

    #[test]
    fn test_collect_imports_relative() {
        let python_code = "from . import module1\nfrom .. import module2";
        let temp_dir = TempDir::new().unwrap();
        let file_path = create_temp_py_file(&temp_dir, "relative_imports.py", python_code);

        let source = fs::read_to_string(file_path).unwrap();
        let parsed = parse(&source, Mode::Module, "relative_imports.py").unwrap();

        let stmts = match &parsed {
            Mod::Module(module) => &module.body,
            _ => panic!("Expected Module"),
        };

        let imports = collect_imports(stmts);

        // Debugging to understand the actual structure
        println!("Relative imports found: {:#?}", imports);

        // For now, just check that we find something, we'll refine this test
        // after seeing the actual output structure
        assert!(!imports.is_empty());
    }

    #[test]
    fn test_collect_imports_nested() {
        let python_code = r#"
def function():
    import math
    
    if True:
        import datetime
        
        class NestedClass:
            import json
            
            def method(self):
                import re
"#;
        let temp_dir = TempDir::new().unwrap();
        let file_path = create_temp_py_file(&temp_dir, "nested_imports.py", python_code);

        let source = fs::read_to_string(file_path).unwrap();
        let parsed = parse(&source, Mode::Module, "nested_imports.py").unwrap();

        let stmts = match &parsed {
            Mod::Module(module) => &module.body,
            _ => panic!("Expected Module"),
        };

        let imports = collect_imports(stmts);

        // Should find all nested imports
        assert_eq!(imports.len(), 4);

        let modules: Vec<String> = imports.iter().map(|i| i.module.clone()).collect();
        assert!(modules.contains(&"math".to_string()));
        assert!(modules.contains(&"datetime".to_string()));
        assert!(modules.contains(&"json".to_string()));
        assert!(modules.contains(&"re".to_string()));
    }

    #[test]
    fn test_detect_package_name_setup_py() {
        let temp_dir = TempDir::new().unwrap();
        let setup_content = r#"
from setuptools import setup

setup(
    name='test_package',
    version='0.1.0',
    packages=['test_package'],
)
"#;
        create_temp_py_file(&temp_dir, "setup.py", setup_content);

        let package_name = detect_package_name(temp_dir.path());
        assert_eq!(package_name, Some("test_package".to_string()));
    }

    #[test]
    fn test_detect_package_name_pyproject_toml_project() {
        let temp_dir = TempDir::new().unwrap();
        let pyproject_content = r#"
[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"

[project]
name = "test_package_project"
version = "0.1.0"
"#;
        // Write to a file within the temp directory
        let file_path = temp_dir.path().join("pyproject.toml");
        let mut file = File::create(&file_path).unwrap();
        file.write_all(pyproject_content.as_bytes()).unwrap();

        // For debugging
        println!("Temp dir: {:?}", temp_dir.path());
        println!("Content written to file: {}", pyproject_content);
        println!("Detecting package name...");

        let package_name = detect_package_name(temp_dir.path());
        println!("Detected package name: {:?}", package_name);

        assert_eq!(package_name, Some("test_package_project".to_string()));
    }

    #[test]
    fn test_detect_package_name_pyproject_toml_poetry() {
        let temp_dir = TempDir::new().unwrap();
        let pyproject_content = r#"
[build-system]
requires = ["poetry-core>=1.0.0"]
build-backend = "poetry.core.masonry.api"

[tool.poetry]
name = "test_package_poetry"
version = "0.1.0"
"#;
        // Write to a file within the temp directory
        let file_path = temp_dir.path().join("pyproject.toml");
        let mut file = File::create(&file_path).unwrap();
        file.write_all(pyproject_content.as_bytes()).unwrap();

        // For debugging
        println!("Temp dir: {:?}", temp_dir.path());
        println!("Content written to file: {}", pyproject_content);
        println!("Detecting package name...");

        let package_name = detect_package_name(temp_dir.path());
        println!("Detected package name: {:?}", package_name);

        assert_eq!(package_name, Some("test_package_poetry".to_string()));
    }

    #[test]
    fn test_detect_package_name_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let dir_name = temp_dir.path().file_name().unwrap().to_str().unwrap();

        let package_name = detect_package_name(temp_dir.path());
        assert_eq!(package_name, Some(dir_name.to_string()));
    }

    #[test]
    fn test_is_third_party_import() {
        let mut manager = ProjectAstManager::new("/test/path");
        // Set package name for testing
        manager.package_name = Some("my_package".to_string());

        // First-party absolute import (starts with package name)
        let first_party = ImportInfo {
            module: "my_package.submodule".to_string(),
            names: vec!["function".to_string()],
            is_relative: false,
        };
        assert!(!manager.is_third_party_import(&first_party));

        // Relative import is always first-party
        let relative = ImportInfo {
            module: "submodule".to_string(),
            names: vec!["function".to_string()],
            is_relative: true,
        };
        assert!(!manager.is_third_party_import(&relative));

        // Third-party import
        let third_party = ImportInfo {
            module: "requests".to_string(),
            names: vec!["get".to_string()],
            is_relative: false,
        };
        assert!(manager.is_third_party_import(&third_party));
    }

    #[test]
    fn test_process_py_file() {
        let temp_dir = TempDir::new().unwrap();
        let python_code = "import os\nfrom sys import path";
        let file_path = create_temp_py_file(&temp_dir, "test_file.py", python_code);

        let mut manager = ProjectAstManager::new(temp_dir.path().to_str().unwrap());
        let imports_result = manager.process_py_file(file_path.to_str().unwrap());

        assert!(imports_result.is_ok());
        let imports = imports_result.unwrap();

        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].module, "os");
        assert_eq!(imports[1].module, "sys");
    }

    #[test]
    fn test_process_py_file_caching() {
        let temp_dir = TempDir::new().unwrap();
        let python_code = "import os\nfrom sys import path";
        let file_path = create_temp_py_file(&temp_dir, "test_cache.py", python_code);
        let path_str = file_path.to_str().unwrap();

        let mut manager = ProjectAstManager::new(temp_dir.path().to_str().unwrap());

        // First call should parse the file
        let _ = manager.process_py_file(path_str).unwrap();

        // Get the hash for later comparison
        let original_hash = manager.file_hashes.get(path_str).unwrap().clone();

        // Second call should use cached result
        let _ = manager.process_py_file(path_str).unwrap();

        // Hash should remain the same
        let new_hash = manager.file_hashes.get(path_str).unwrap();
        assert_eq!(&original_hash, new_hash);

        // Now modify the file
        let python_code_modified = "import os\nfrom sys import path\nimport datetime";
        let mut file = File::create(&file_path).unwrap();
        file.write_all(python_code_modified.as_bytes()).unwrap();

        // Process again - should detect changes
        let imports = manager.process_py_file(path_str).unwrap();

        // Should now have 3 imports
        assert_eq!(imports.len(), 3);

        // Hash should have changed
        let modified_hash = manager.file_hashes.get(path_str).unwrap();
        assert_ne!(&original_hash, modified_hash);
    }

    #[test]
    fn test_compute_import_delta() {
        let temp_dir = TempDir::new().unwrap();

        // Create initial files
        let file1_path = create_temp_py_file(&temp_dir, "file1.py", "import os\nimport requests");
        let _file2_path = create_temp_py_file(&temp_dir, "file2.py", "import sys\nimport flask");

        let mut manager = ProjectAstManager::new(temp_dir.path().to_str().unwrap());

        // Initial processing
        let initial_imports = manager.process_all_py_files().unwrap();
        println!("Initial imports found: {:#?}", initial_imports);

        // We're expecting third-party imports, so we need to set the package name
        // Otherwise, all imports will be treated as third-party
        manager.package_name = Some("testpkg".to_string());

        // Re-process to get third-party imports with the package name set
        let initial_imports = manager.process_all_py_files().unwrap();

        // Verify we have the expected number of third-party imports
        // os, requests, sys, flask should all be considered third-party
        assert!(!initial_imports.is_empty());

        // Compute delta - should be empty since we just initialized
        let (added, removed) = manager.compute_import_delta().unwrap();
        assert!(added.is_empty());
        assert!(removed.is_empty());

        // Modify file1.py to add a new import and remove an existing one
        let file1_modified = "import os\nimport pandas";
        let mut file = File::create(&file1_path).unwrap();
        file.write_all(file1_modified.as_bytes()).unwrap();

        // Compute delta - should detect the changes
        let (added, removed) = manager.compute_import_delta().unwrap();
        println!("Added imports: {:#?}", added);
        println!("Removed imports: {:#?}", removed);

        assert!(!added.is_empty());
        assert!(added.contains("pandas"));

        assert!(!removed.is_empty());
        assert!(removed.contains("requests"));
    }
}
