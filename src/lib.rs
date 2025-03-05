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

// Define our messages module
pub mod messages;
pub mod environment;
pub mod scripts;

// Export types from messages and scripts for public use
pub use messages::{Message, ForkRequest, ExitRequest};
use scripts::{PYTHON_LOADER_SCRIPT, PYTHON_CALL_SCRIPT};

/// A simple structure to hold information about an import.
#[derive(Debug)]
struct ImportInfo {
    /// For an `import X`, this is "X". For a `from X import Y`, this is "X".
    module: String,
    /// The names imported from that module.
    names: Vec<String>,
    /// Whether this is a relative import (starts with . or ..)
    is_relative: bool,
}

/// Recursively traverse AST statements to collect import information.
/// Absolute (level == 0) imports are considered third-party.
fn collect_imports(stmts: &[Stmt]) -> Vec<ImportInfo> {
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

/// Given a path, scan for all Python files, parse them and extract the set of
/// absolute (non-relative) modules that are imported.
fn process_py_files(path: &Path) -> Result<(HashSet<String>, Option<String>)> {
    let mut third_party_modules = HashSet::new();
    let package_name = detect_package_name(path);

    println!("Detected package name: {:?}", package_name);

    for entry in WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file() && e.path().extension().map(|ext| ext == "py").unwrap_or(false)
        })
    {
        let file_path = entry.path();
        let source = fs::read_to_string(file_path)?;
        let parsed = parse(&source, Mode::Module, file_path.to_string_lossy().as_ref())
            .map_err(|e| anyhow!("Failed to parse {}: {:?}", file_path.display(), e))?;
        // Extract statements from the module. Note: `Mod::Module` now has named fields.
        let stmts: &[Stmt] = match &parsed {
            Mod::Module(module) => &module.body,
            _ => {
                return Err(anyhow!(
                    "Unexpected AST format for module in file {}",
                    file_path.display()
                ))
            }
        };
        let imports = collect_imports(stmts);
        for imp in imports {
            // Skip relative imports and imports of the current package
            if !imp.is_relative
                && !package_name
                    .as_ref()
                    .map_or(false, |pkg| imp.module.starts_with(pkg))
            {
                third_party_modules.insert(imp.module);
            }
        }
    }
    Ok((third_party_modules, package_name))
}

/// Spawn a Python process that imports the given modules and then waits for commands on stdin.
/// The Python process prints "IMPORTS_LOADED" to stdout once all imports are complete.
/// After that, it will listen for commands on stdin, which can include fork requests and code to execute.
fn spawn_python_loader(modules: &HashSet<String>) -> Result<Child> {
    // Create import code for Python to execute
    let mut import_lines = String::new();
    for module in modules {
        import_lines.push_str(&format!(
            "try:\n    __import__('{}')\nexcept ImportError as e:\n    print('Failed to import {}:', e)\n",
            module, module
        ));
    }

    // Write the embedded loader script to a file in the /tmp directory
    println!("Writing Python loader script to /tmp directory");
    let temp_path =
        std::path::Path::new("/tmp").join(format!("hotreload_loader_{}.py", std::process::id()));
    std::fs::write(&temp_path, PYTHON_LOADER_SCRIPT.as_bytes())
        .map_err(|e| anyhow!("Failed to write Python loader script to /tmp file: {}", e))?;

    // Launch the Python process with the loader script
    let child = Command::new("python")
        .arg(&temp_path)
        .arg(import_lines)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn python process: {}", e))?;

    Ok(child)
}

// Replace single IMPORT_RUNNER with a registry of runners
static IMPORT_RUNNERS: Lazy<Mutex<HashMap<String, environment::ImportRunner>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Python module for hot reloading with isolated imports
#[pymodule]
fn hotreload(_py: Python, m: &PyModule) -> PyResult<()> {
    // Register the module functions
    m.add_function(wrap_pyfunction!(start_import_runner, m)?)?;
    m.add_function(wrap_pyfunction!(stop_import_runner, m)?)?;
    m.add_function(wrap_pyfunction!(exec_isolated, m)?)?;
    m.add_function(wrap_pyfunction!(stop_isolated, m)?)?;
    m.add_function(wrap_pyfunction!(communicate_isolated, m)?)?;

    Ok(())
}

/// Initialize and start the import runner, returning a unique identifier
#[pyfunction]
fn start_import_runner(_py: Python, package_path: &str) -> PyResult<String> {
    // Process Python files
    let (modules, package_name) = process_py_files(Path::new(package_path))
        .map_err(|e| PyRuntimeError::new_err(format!("Failed to process Python files: {}", e)))?;

    let _package_name =
        package_name.ok_or_else(|| PyRuntimeError::new_err("Could not determine package name"))?;

    // Spawn Python subprocess to load modules
    let mut child = spawn_python_loader(&modules)
        .map_err(|e| PyRuntimeError::new_err(format!("Failed to spawn Python loader: {}", e)))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| PyRuntimeError::new_err("Failed to capture stdin for python process"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| PyRuntimeError::new_err("Failed to capture stdout for python process"))?;

    let reader = BufReader::new(stdout);
    let mut lines_iter = reader.lines();

    // Wait for the ImportComplete message
    let mut imports_loaded = false;
    for line in &mut lines_iter {
        let line =
            line.map_err(|e| PyRuntimeError::new_err(format!("Failed to read line: {}", e)))?;

        // Parse the line as a message
        if let Ok(message) = serde_json::from_str::<Message>(&line) {
            match message {
                Message::ImportComplete(_) => {
                    imports_loaded = true;
                    break;
                }
                Message::ImportError(error) => {
                    return Err(PyRuntimeError::new_err(format!(
                        "Import error: {}",
                        error.error
                    )));
                }
                _ => {
                    // Print other message types for debugging
                    println!("Received message: {}", line);
                }
            }
        } else {
            // If we can't parse it as a message, log it
            println!("Non-message output: {}", line);
        }
    }

    if !imports_loaded {
        return Err(PyRuntimeError::new_err(
            "Python loader did not report successful imports",
        ));
    }

    // Generate a unique identifier for this runner
    let runner_id = Uuid::new_v4().to_string();

    // Create the runner object
    let runner = environment::ImportRunner {
        id: runner_id.clone(),
        child: Arc::new(Mutex::new(child)),
        stdin: Arc::new(Mutex::new(stdin)),
        reader: Arc::new(Mutex::new(lines_iter)),
        forked_processes: Arc::new(Mutex::new(HashMap::new())),
    };

    // Store in global registry
    let mut runners = IMPORT_RUNNERS.lock().unwrap();
    runners.insert(runner_id.clone(), runner);

    Ok(runner_id)
}

/// Stop the import runner with the given ID
#[pyfunction]
fn stop_import_runner(_py: Python, runner_id: &str) -> PyResult<()> {
    let mut runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.remove(runner_id) {
        // Clean up resources
        runner.stop_main().map_err(|e| PyRuntimeError::new_err(format!("Failed to stop import runner: {}", e)))?;
        Ok(())
    } else {
        Err(PyRuntimeError::new_err(format!(
            "No import runner found with ID: {}",
            runner_id
        )))
    }
}

/// Execute code with a specific runner
#[pyfunction]
fn exec_isolated<'py>(
    py: Python<'py>,
    runner_id: &str,
    func: PyObject,
    args: Option<PyObject>,
) -> PyResult<&'py PyAny> {
    // Create a dict to hold our function and args for pickling
    let locals = PyDict::new(py);
    locals.set_item("func", func)?;
    locals.set_item("args", args.unwrap_or_else(|| py.None()))?;

    py.run(PYTHON_CALL_SCRIPT, None, Some(locals))?;

    let func_module_path = locals
        .get_item("func_module_path")
        .ok_or_else(|| PyRuntimeError::new_err("Failed to get function module path"))?
        .extract::<String>()?;

    let func_file_path = locals
        .get_item("func_file_path")
        .ok_or_else(|| PyRuntimeError::new_err("Failed to get function file path"))?
        .extract::<String>()?;

    // Pickle the function, args, and module path and encode in base64
    let pickle_code = "import pickle, base64; pickled_data = base64.b64encode(pickle.dumps((func, args, func_module_path))).decode('utf-8')";
    py.run(pickle_code, None, Some(locals))?;

    // Get the pickled data - now it's a string because we decoded it in Python
    let pickled_data = locals
        .get_item("pickled_data")
        .ok_or_else(|| PyRuntimeError::new_err("Failed to pickle function and args"))?
        .extract::<String>()?;

    let runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.get(runner_id) {
        // Convert Rust Result<String, String> to PyResult
        match runner.exec_isolated(&func_module_path, &func_file_path, &pickled_data) {
            Ok(result) => Ok(py.eval(&format!("'{}'", result), None, None)?),
            Err(err) => Err(PyRuntimeError::new_err(err)),
        }
    } else {
        Err(PyRuntimeError::new_err(format!(
            "No import runner found with ID: {}",
            runner_id
        )))
    }
}

/// Stop an isolated process by ID
#[pyfunction]
fn stop_isolated(_py: Python, runner_id: &str, process_uuid: &str) -> PyResult<bool> {
    let runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.get(runner_id) {
        runner
            .stop_isolated(process_uuid)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to stop isolated process: {}", e)))
    } else {
        Err(PyRuntimeError::new_err(format!(
            "No import runner found with ID: {}",
            runner_id
        )))
    }
}

/// Communicate with an isolated process to get its output
#[pyfunction]
fn communicate_isolated(
    _py: Python,
    runner_id: &str,
    process_uuid: &str,
) -> PyResult<Option<String>> {
    let runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.get(runner_id) {
        runner.communicate_isolated(process_uuid).map_err(|e| {
            PyRuntimeError::new_err(format!(
                "Failed to communicate with isolated process: {}",
                e
            ))
        })
    } else {
        Err(PyRuntimeError::new_err(format!(
            "No import runner found with ID: {}",
            runner_id
        )))
    }
}
