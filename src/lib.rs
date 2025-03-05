use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::fs;
use walkdir::WalkDir;
use tempfile::NamedTempFile;
use rustpython_parser::{parser, ast};
use std::collections::HashSet;

/// A Python module implemented in Rust.
#[pymodule]
fn hotreload(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(sum_as_string, m)?)?;
    m.add_function(wrap_pyfunction!(analyze_and_snapshot_imports, m)?)?;
    m.add_function(wrap_pyfunction!(measure_imports_memory_usage, m)?)?;
    Ok(())
}

/// Formats the sum of two numbers as string.
#[pyfunction]
fn sum_as_string(a: usize, b: usize) -> PyResult<String> {
    Ok((a + b).to_string())
}

/// Analyzes Python files in a directory, extracts third-party imports,
/// loads them in a separate Python process, and takes a memory snapshot.
///
/// Args:
///     path: Directory path containing Python files to analyze
///     output_path: Path where the memory snapshot will be saved
///     exclude_dirs: Optional list of directory names to exclude
///
/// Returns:
///     Dictionary with analysis results including found imports and snapshot path
#[pyfunction]
fn analyze_and_snapshot_imports(
    py: Python,
    path: String,
    output_path: Option<String>,
    exclude_dirs: Option<Vec<String>>,
) -> PyResult<PyObject> {
    // Create default output path if not provided
    let snapshot_path = match output_path {
        Some(p) => p,
        None => format!("{}/memory_snapshot.json", path),
    };
    
    // Set up exclude directories
    let exclude_dirs = exclude_dirs.unwrap_or_else(|| vec![
        ".git".to_string(),
        ".venv".to_string(),
        "venv".to_string(),
        "__pycache__".to_string(),
        "node_modules".to_string(),
    ]);
    
    // Find all Python files in the directory
    let mut py_files = Vec::new();
    for entry in WalkDir::new(&path)
        .follow_links(true)
        .into_iter()
        .filter_entry(|e| {
            let file_name = e.file_name().to_string_lossy();
            !exclude_dirs.iter().any(|d| file_name == *d)
        })
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_file() && path.extension().map_or(false, |ext| ext == "py") {
            py_files.push(path.to_string_lossy().to_string());
        }
    }
    
    // Create a Python script to analyze imports
    let temp_script = create_analyzer_script(py, &py_files)?;
    let temp_script_path = temp_script.path().to_string_lossy().to_string();
    
    // Run the analyzer script to get third-party imports
    let output = Command::new("python")
        .arg(&temp_script_path)
        .output()
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
            format!("Failed to execute Python process: {}", e)
        ))?;
    
    if !output.status.success() {
        return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
            format!("Analyzer script failed: {}", String::from_utf8_lossy(&output.stderr))
        ));
    }
    
    // Parse the output to get the imports
    let imports: Vec<String> = serde_json::from_slice(&output.stdout)
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyValueError, _>(
            format!("Failed to parse analyzer output: {}", e)
        ))?;
    
    // Create a script to load modules and take memory snapshot
    let snapshot_script = create_snapshot_script(py, &imports, &snapshot_path)?;
    let snapshot_script_path = snapshot_script.path().to_string_lossy().to_string();
    
    // Run the snapshot script
    let snapshot_output = Command::new("python")
        .arg(&snapshot_script_path)
        .output()
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
            format!("Failed to execute snapshot process: {}", e)
        ))?;
    
    if !snapshot_output.status.success() {
        return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
            format!("Snapshot script failed: {}", String::from_utf8_lossy(&snapshot_output.stderr))
        ));
    }
    
    // Create result dictionary
    let result = PyDict::new(py);
    result.set_item("imports", imports)?;
    result.set_item("snapshot_path", &snapshot_path)?;
    result.set_item("analyzed_files", py_files)?;
    
    Ok(result.into())
}

/// Measures memory usage of third-party imports in Python files
/// 
/// Args:
///     path: Directory path to scan for Python files
///     output_path: Optional custom path for the memory snapshot file
///     exclude_dirs: Optional list of directories to exclude from scanning
///
/// Returns:
///     Dictionary with memory usage data and third-party imports
#[pyfunction]
fn measure_imports_memory_usage(
    py: Python,
    path: String,
    output_path: Option<String>,
    exclude_dirs: Option<Vec<String>>,
) -> PyResult<PyObject> {
    // Create default output path if not provided
    let snapshot_path = match output_path {
        Some(p) => p,
        None => format!("{}/imports_memory_snapshot.json", path),
    };
    
    // Set up exclude directories
    let exclude_dirs = exclude_dirs.unwrap_or_else(|| vec![
        ".git".to_string(),
        ".venv".to_string(),
        "venv".to_string(),
        "__pycache__".to_string(),
        "node_modules".to_string(),
    ]);
    
    // Find all Python files in the directory
    let mut py_files = Vec::new();
    for entry in WalkDir::new(&path)
        .follow_links(true)
        .into_iter()
        .filter_entry(|e| {
            let file_name = e.file_name().to_string_lossy();
            !exclude_dirs.iter().any(|d| file_name == *d)
        })
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_file() && path.extension().map_or(false, |ext| ext == "py") {
            py_files.push(path.to_string_lossy().to_string());
        }
    }
    
    // Extract third-party imports from all Python files
    let mut third_party_modules = HashSet::new();
    
    for py_file in &py_files {
        match fs::read_to_string(py_file) {
            Ok(content) => {
                match parser::parse_program(&content, &py_file) {
                    Ok(program) => {
                        // Extract imports from AST
                        for stmt in program.statements {
                            match stmt.node {
                                ast::StatementType::Import { names } => {
                                    for alias in names {
                                        let module_name = alias.node.name;
                                        // Exclude standard library modules (simplified approach)
                                        if !is_standard_library(&module_name) {
                                            third_party_modules.insert(module_name);
                                        }
                                    }
                                },
                                ast::StatementType::ImportFrom { level: _, module, names } => {
                                    if let Some(module_name) = module {
                                        // Exclude standard library modules
                                        if !is_standard_library(&module_name) {
                                            third_party_modules.insert(module_name);
                                        }
                                    }
                                },
                                _ => {}
                            }
                        }
                    },
                    Err(err) => {
                        eprintln!("Error parsing {}: {}", py_file, err);
                    }
                }
            },
            Err(err) => {
                eprintln!("Error reading {}: {}", py_file, err);
            }
        }
    }
    
    // Create a Python script that will import these modules and measure memory
    let import_list: Vec<String> = third_party_modules.into_iter().collect();
    let temp_file = create_memory_measurement_script(py, &import_list, &snapshot_path)?;
    let temp_path = temp_file.path().to_string_lossy().to_string();
    
    // Run the memory measurement script
    let output = Command::new("python")
        .arg(&temp_path)
        .output()
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
            format!("Failed to execute memory measurement script: {}", e)
        ))?;
    
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
            format!("Memory measurement script failed: {}", stderr)
        ));
    }
    
    // Parse the output JSON
    let output_json = fs::read_to_string(&snapshot_path)
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyIOError, _>(
            format!("Failed to read memory snapshot: {}", e)
        ))?;
    
    let result: serde_json::Value = serde_json::from_str(&output_json)
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyValueError, _>(
            format!("Failed to parse memory snapshot: {}", e)
        ))?;
    
    // Return the result as a Python dictionary
    let py_result = result.to_string().into_py(py);
    Ok(py_result)
}

/// Helper function to determine if a module is part of the Python standard library
fn is_standard_library(module_name: &str) -> bool {
    // List of common standard library modules
    // This is a simplified approach - a more comprehensive approach would check against
    // the full list of standard library modules for the specific Python version
    let std_libs = [
        "abc", "argparse", "asyncio", "collections", "contextlib", "copy", "csv", "datetime",
        "enum", "functools", "glob", "io", "json", "logging", "math", "os", "pathlib", "random", 
        "re", "socket", "string", "sys", "time", "typing", "unittest", "uuid"
    ];
    
    // Check if the module name or its root package is in the standard library
    let root_package = module_name.split('.').next().unwrap_or(module_name);
    std_libs.contains(&root_package)
}

/// Creates a Python script that imports specified modules and measures memory usage
fn create_memory_measurement_script(py: Python, imports: &[String], output_path: &str) -> PyResult<NamedTempFile> {
    let mut temp_file = NamedTempFile::new().map_err(|e| {
        PyErr::new::<pyo3::exceptions::PyIOError, _>(format!("Failed to create temp file: {}", e))
    })?;
    
    // Generate import statements
    let import_statements = imports
        .iter()
        .map(|module| format!("try:\n    import {}\nexcept ImportError:\n    print(f\"Could not import {{{}}}\")", module, module))
        .collect::<Vec<_>>()
        .join("\n");
    
    // Create a script that:
    // 1. Imports all the specified modules
    // 2. Measures memory usage
    // 3. Saves the results to a JSON file
    let script = py.eval(
        &format!(
            r#"
import os
import sys
import json
import gc
import time
import tracemalloc

def take_memory_snapshot(output_path):
    # Start tracking memory allocations
    tracemalloc.start()
    
    # Force garbage collection before measuring
    gc.collect()
    
    # Import the modules
    print("Importing modules...")
    {}
    
    # Give time for imports to fully initialize
    time.sleep(1)
    gc.collect()
    
    # Get memory snapshot
    snapshot = tracemalloc.take_snapshot()
    top_stats = snapshot.statistics('lineno')
    
    # Prepare report
    report = {{
        "timestamp": time.time(),
        "total_memory_mb": tracemalloc.get_traced_memory()[1] / (1024 * 1024),
        "top_memory_consumers": [
            {{
                "filename": stat.traceback[0].filename,
                "lineno": stat.traceback[0].lineno,
                "size_kb": stat.size / 1024
            }}
            for stat in top_stats[:50]  # Top 50 memory consumers
        ],
        "imported_modules": {}
    }}
    
    # Save report to JSON
    os.makedirs(os.path.dirname(os.path.abspath(output_path)), exist_ok=True)
    with open(output_path, 'w') as f:
        json.dump(report, f, indent=2)
    
    print(f"Memory snapshot saved to {{output_path}}")
    return report

# Run the snapshot function
take_memory_snapshot('{}')"#,
            import_statements,
            output_path
        ),
        None,
        None,
    )?;
    
    let script_content = script.extract::<String>()?;
    fs::write(temp_file.path(), script_content).map_err(|e| {
        PyErr::new::<pyo3::exceptions::PyIOError, _>(format!("Failed to write temp file: {}", e))
    })?;
    
    Ok(temp_file)
}

// Helper function to create a temporary Python script for analyzing imports
fn create_analyzer_script(py: Python, py_files: &[String]) -> PyResult<NamedTempFile> {
    let temp_file = NamedTempFile::new().map_err(|e| {
        PyErr::new::<pyo3::exceptions::PyIOError, _>(format!("Failed to create temp file: {}", e))
    })?;
    
    let script = py.eval(
        r#"
import ast
import json
import sys

def get_imports(file_path):
    try:
        with open(file_path, 'r', encoding='utf-8') as f:
            content = f.read()
        
        tree = ast.parse(content)
        imports = []
        
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                for name in node.names:
                    imports.append(name.name.split('.')[0])
            elif isinstance(node, ast.ImportFrom):
                if node.module is not None:
                    imports.append(node.module.split('.')[0])
        
        return imports
    except Exception as e:
        print(f"Error parsing {file_path}: {e}", file=sys.stderr)
        return []

def is_third_party(module_name):
    # Skip standard library modules
    stdlib_modules = set([
        'abc', 'argparse', 'ast', 'asyncio', 'base64', 'collections', 'concurrent',
        'contextlib', 'copy', 'csv', 'datetime', 'decimal', 'difflib', 'enum',
        'functools', 'glob', 'gzip', 'hashlib', 'http', 'importlib', 'inspect',
        'io', 'itertools', 'json', 'logging', 'math', 'multiprocessing', 'os',
        'pathlib', 'pickle', 'random', 're', 'shutil', 'signal', 'socket',
        'sqlite3', 'statistics', 'string', 'subprocess', 'sys', 'tempfile',
        'threading', 'time', 'traceback', 'typing', 'unittest', 'urllib', 'uuid',
        'warnings', 'weakref', 'xml', 'zipfile'
    ])
    
    # Skip local modules (those that start with underscore or are local to the project)
    if module_name.startswith('_') or '.' in module_name:
        return False
    
    return module_name not in stdlib_modules

# Get file paths from command line arguments
file_paths = sys.argv[1:]

# Collect all imports
all_imports = []
for file_path in file_paths:
    all_imports.extend(get_imports(file_path))

# Filter to third-party modules and remove duplicates
third_party_imports = list(set(filter(is_third_party, all_imports)))

# Output as JSON
print(json.dumps(third_party_imports))
        "#,
        None,
        None,
    )?;
    
    let script_content = script.extract::<String>()?;
    fs::write(temp_file.path(), script_content).map_err(|e| {
        PyErr::new::<pyo3::exceptions::PyIOError, _>(format!("Failed to write temp file: {}", e))
    })?;
    
    // Append file paths as arguments
    let mut command = Command::new("python");
    command.arg(temp_file.path());
    for file_path in py_files {
        command.arg(file_path);
    }
    
    Ok(temp_file)
}

// Helper function to create a temporary Python script for loading modules and taking a memory snapshot
fn create_snapshot_script(py: Python, imports: &[String], output_path: &str) -> PyResult<NamedTempFile> {
    let temp_file = NamedTempFile::new().map_err(|e| {
        PyErr::new::<pyo3::exceptions::PyIOError, _>(format!("Failed to create temp file: {}", e))
    })?;
    
    let imports_json = serde_json::to_string(imports).map_err(|e| {
        PyErr::new::<pyo3::exceptions::PyValueError, _>(format!("Failed to serialize imports: {}", e))
    })?;
    
    let script = py.eval(
        &format!(r#"
import json
import sys
import os
import time
import tracemalloc
import gc

def take_memory_snapshot(output_path):
    # Get the imports from the JSON string
    imports = json.loads('{imports_json}')
    
    # Force garbage collection before starting
    gc.collect()
    
    # Start tracing memory allocations
    tracemalloc.start()
    
    # Import each module
    successfully_loaded = []
    failed_imports = []
    
    for module_name in imports:
        try:
            print(f"Loading module: {{module_name}}")
            __import__(module_name)
            successfully_loaded.append(module_name)
        except Exception as e:
            failed_imports.append({{
                "module": module_name,
                "error": str(e)
            }})
            print(f"Failed to import {{module_name}}: {{e}}", file=sys.stderr)
    
    # Take snapshot after all imports
    snapshot = tracemalloc.take_snapshot()
    
    # Get top memory consumers
    top_stats = snapshot.statistics('lineno')
    
    # Create a report
    report = {{
        "timestamp": time.time(),
        "successfully_loaded": successfully_loaded,
        "failed_imports": failed_imports,
        "memory_stats": [
            {{
                "file": stat.traceback[0].filename,
                "line": stat.traceback[0].lineno,
                "size": stat.size,
                "count": stat.count
            }}
            for stat in top_stats[:100]  # Top 100 memory consumers
        ],
        "total_memory": sum(stat.size for stat in top_stats)
    }}
    
    # Save to file
    os.makedirs(os.path.dirname(os.path.abspath(output_path)), exist_ok=True)
    with open(output_path, 'w') as f:
        json.dump(report, f, indent=2)
    
    print(f"Memory snapshot saved to {{output_path}}")
    return report

# Run the snapshot function
take_memory_snapshot('{output_path}')
        "#),
        None,
        None,
    )?;
    
    let script_content = script.extract::<String>()?;
    fs::write(temp_file.path(), script_content).map_err(|e| {
        PyErr::new::<pyo3::exceptions::PyIOError, _>(format!("Failed to write temp file: {}", e))
    })?;
    
    Ok(temp_file)
} 