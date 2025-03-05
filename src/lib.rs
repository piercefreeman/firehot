use anyhow::{anyhow, Result};
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};
use walkdir::WalkDir;

use rustpython_parser::{parse, Mode};
use rustpython_parser::ast::{
    Mod, Stmt,
    StmtIf, StmtWhile, StmtFunctionDef, StmtAsyncFunctionDef, StmtClassDef,
};

// Add PyO3 imports for Python bindings
use pyo3::prelude::*;
use pyo3::exceptions::PyRuntimeError;
use pyo3::types::{PyBytes, PyDict};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use once_cell::sync::Lazy;

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
                let if_stmt: &StmtIf = &*inner;
                imports.extend(collect_imports(&if_stmt.body));
                imports.extend(collect_imports(&if_stmt.orelse));
            }
            Stmt::While(inner) => {
                let while_stmt: &StmtWhile = &*inner;
                imports.extend(collect_imports(&while_stmt.body));
                imports.extend(collect_imports(&while_stmt.orelse));
            }
            Stmt::FunctionDef(inner) => {
                let func_def: &StmtFunctionDef = &*inner;
                imports.extend(collect_imports(&func_def.body));
            }
            Stmt::AsyncFunctionDef(inner) => {
                let func_def: &StmtAsyncFunctionDef = &*inner;
                imports.extend(collect_imports(&func_def.body));
            }
            Stmt::ClassDef(inner) => {
                let class_def: &StmtClassDef = &*inner;
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
                let name_re = regex::Regex::new(r#"(?:\[project\]|\[tool\.poetry\]).*?name\s*=\s*["']([^"']+)["']"#).unwrap();
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
            e.file_type().is_file()
                && e.path().extension().map(|ext| ext == "py").unwrap_or(false)
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
            if !imp.is_relative && 
               !package_name.as_ref().map_or(false, |pkg| imp.module.starts_with(pkg)) {
                third_party_modules.insert(imp.module);
            }
        }
    }
    Ok((third_party_modules, package_name))
}

// Embed the Python loader script directly in the binary
const PYTHON_LOADER_SCRIPT: &str = include_str!("../python/loader.py");

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
    let temp_path = std::path::Path::new("/tmp").join(format!("hotreload_loader_{}.py", std::process::id()));
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

/// Runner for isolated Python code execution
#[pyclass]
struct ImportRunner {
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<std::process::ChildStdin>>,
    reader: Arc<Mutex<std::io::Lines<BufReader<std::process::ChildStdout>>>>,
}

#[pymethods]
impl ImportRunner {
    /// Execute a function in the isolated environment
    #[pyo3(text_signature = "(func, args, /)")]
    fn exec<'py>(
        &self, 
        py: Python<'py>, 
        func: PyObject, 
        args: Option<PyObject>
    ) -> PyResult<&'py PyAny> {
        // Create a dict to hold our function and args for pickling
        let locals = PyDict::new(py);
        locals.set_item("func", func)?;
        locals.set_item("args", args.unwrap_or_else(|| py.None()))?;
        
        // Pickle the function and args
        let pickle_code = "import pickle; pickled_data = pickle.dumps((func, args))";
        py.run(pickle_code, None, Some(locals))?;
        
        // Get the pickled data
        let pickled_data = locals.get_item("pickled_data")
            .ok_or_else(|| PyRuntimeError::new_err("Failed to pickle function and args"))?;
        let _pickled_bytes = pickled_data.downcast::<PyBytes>()
            .map_err(|_| PyRuntimeError::new_err("Pickled data is not bytes"))?;
        
        // Create the Python execution code that will unpickle and run the function
        let exec_code = format!(
            r#"
import pickle
import base64

# Base64 encode for safe transmission
pickled_bytes = {}
data = pickle.loads(pickled_bytes)
func, args = data

# Run the function with args
if isinstance(args, tuple):
    result = func(*args)
elif args is not None:
    result = func(args)
else:
    result = func()
            "#, 
            py.eval("repr(pickled_data)", None, None)?
        );
        
        // Send the code to the forked process
        let mut stdin_guard = self.stdin.lock()
            .map_err(|_| PyRuntimeError::new_err("Failed to lock stdin mutex"))?;
        writeln!(stdin_guard, "FORK:{}", exec_code)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to write to child process: {}", e)))?;
        drop(stdin_guard);
        
        // Wait for response
        let mut reader_guard = self.reader.lock()
    .map_err(|_| PyRuntimeError::new_err("Failed to lock reader mutex"))?;

        
    for line in &mut *reader_guard {
        let line = line.map_err(|e| PyRuntimeError::new_err(format!("Failed to read line: {}", e)))?;
            
            if line.starts_with("FORKED:") {
                let fork_pid = line[7..].parse::<i32>()
                    .map_err(|_| PyRuntimeError::new_err("Invalid fork PID"))?;
                py.allow_threads(|| {
                    // Wait a bit for the fork to complete to avoid race conditions
                    std::thread::sleep(Duration::from_millis(100));
                });
                return Ok(py.eval(
                    &format!("print('Function running in forked process (PID: {}')", fork_pid), 
                    None, None
                )?);
            }
            
            if line.starts_with("FORK_RUN_ERROR:") {
                let error_json = &line[14..];
                return Err(PyRuntimeError::new_err(format!("Function execution failed: {}", error_json)));
            }
            
            if line.starts_with("ERROR:") {
                let error_msg = &line[6..];
                return Err(PyRuntimeError::new_err(format!("Process error: {}", error_msg)));
            }
        }
        
        Err(PyRuntimeError::new_err("Unexpected end of output from child process"))
    }
}

/// Python module for hot reloading with isolated imports
#[pymodule]
fn hotreload(_py: Python, m: &PyModule) -> PyResult<()> {
    // Register the module functions
    m.add_function(wrap_pyfunction!(start_import_runner, m)?)?;
    m.add_function(wrap_pyfunction!(stop_import_runner, m)?)?;
    // m.add_function(wrap_pyfunction!(isolate_imports, m)?)?;
    
    Ok(())
}

// Global storage for the import runner
static IMPORT_RUNNER: Lazy<Mutex<Option<ImportRunner>>> = Lazy::new(|| Mutex::new(None));

/// Initialize and start the import runner
#[pyfunction]
fn start_import_runner(py: Python, package_path: &str) -> PyResult<()> {
    // Process Python files
    let (modules, package_name) = process_py_files(Path::new(package_path))
        .map_err(|e| PyRuntimeError::new_err(format!("Failed to process Python files: {}", e)))?;
    
    let package_name = package_name.ok_or_else(|| 
        PyRuntimeError::new_err("Could not determine package name"))?;
    
    // Spawn Python subprocess to load modules
    let mut child = spawn_python_loader(&modules)
        .map_err(|e| PyRuntimeError::new_err(format!("Failed to spawn Python loader: {}", e)))?;
    
    let stdin = child.stdin.take()
        .ok_or_else(|| PyRuntimeError::new_err("Failed to capture stdin for python process"))?;
    
    let stdout = child.stdout.take()
        .ok_or_else(|| PyRuntimeError::new_err("Failed to capture stdout for python process"))?;
    
    let reader = BufReader::new(stdout);
    let mut lines_iter = reader.lines();
    
    // Wait for the IMPORTS_LOADED message
    let mut imports_loaded = false;
    for line in &mut lines_iter {
        let line = line.map_err(|e| PyRuntimeError::new_err(format!("Failed to read line: {}", e)))?;
        if line == "IMPORTS_LOADED" {
            imports_loaded = true;
            break;
        }
        // Print any other messages (like import errors)
        println!("{}", line);
    }
    
    if !imports_loaded {
        return Err(PyRuntimeError::new_err("Python loader did not report successful imports"));
    }
    
    // Create the runner object with the existing reader instead of trying to create a new one
    //let reader = lines_iter.into_iter();
    
    // Create the runner object
    let runner = ImportRunner {
        child: Arc::new(Mutex::new(child)),
        stdin: Arc::new(Mutex::new(stdin)),
        reader: Arc::new(Mutex::new(lines_iter)),
    };
    
    // Store in global storage
    let mut global_runner = IMPORT_RUNNER.lock().unwrap();
    *global_runner = Some(runner);
    
    Ok(())
}

/// Stop the import runner
#[pyfunction]
fn stop_import_runner(_py: Python) -> PyResult<()> {
    let mut global_runner = IMPORT_RUNNER.lock().unwrap();
    if let Some(runner) = global_runner.take() {
        // Clean up resources
        let mut child = runner.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
    
    Ok(())
}

/*#[pyfunction]
fn isolate_imports(py: Python, package_path: &str) -> PyResult<PyObject> {
    // First, ensure we have a runner available
    {
        let global_runner = IMPORT_RUNNER.lock().unwrap();
        if global_runner.is_none() {
            start_import_runner(py, package_path)?;
        }
    }
    
    // Create a Python module with the functions needed
    let sys = py.import("sys")?;
    let builtins = py.import("builtins")?;
    let importlib = py.import("importlib")?;
    
    // Return the Python module with utilities
    let module = PyModule::new(py, "hotreload_utils")?;
    module.add("sys", sys)?;
    module.add("builtins", builtins)?;
    module.add("importlib", importlib)?;
    
    Ok(module.into())
}

/// Main function tying all steps together.
pub fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        return Err(anyhow!("Usage: {} <path_to_scan>", args[0]));
    }
    let scan_path = PathBuf::from(&args[1]);

    // 1. Process Python files.
    let (modules, package_name) = process_py_files(&scan_path)?;
    println!("Package name: {:?}", package_name);
    println!("Found third-party modules to load: {:?}", modules);

    // 2. Spawn a Python process to load these modules.
    let mut child = spawn_python_loader(&modules)?;

    // 3. Read stdout until we see "IMPORTS_LOADED".
    let stdout = child.stdout.take()
        .ok_or_else(|| anyhow!("Failed to capture stdout from python process"))?;
    let mut stdin = child.stdin.take()
        .ok_or_else(|| anyhow!("Failed to capture stdin for python process"))?;
    
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    
    // Wait for imports to complete
    loop {
        if let Some(Ok(line)) = lines.next() {
            println!("Python process: {}", line);
            if line.trim() == "IMPORTS_LOADED" {
                break;
            }
        } else {
            return Err(anyhow!("Python process terminated unexpectedly before imports completed"));
        }
    }

    // 4. Demonstrate forking and executing code
    println!("Sending code to first child process...");
    writeln!(stdin, "FORK:print('Hello from child process 1')")
        .map_err(|e| anyhow!("Failed to write to stdin: {}", e))?;
    
    // Wait for response from the fork
    let mut fork_complete = false;
    while !fork_complete {
        if let Some(Ok(line)) = lines.next() {
            println!("Python process: {}", line);
            if line.starts_with("FORKED:") {
                // Successfully forked
                let fork_pid = line.trim_start_matches("FORKED:");
                println!("Forked child process with PID: {}", fork_pid);
            } else if line.starts_with("FORK_RUN_COMPLETE:") {
                fork_complete = true;
            } else if line.starts_with("FORK_RUN_ERROR:") {
                return Err(anyhow!("Error in child process: {}", 
                    line.trim_start_matches("FORK_RUN_ERROR:")));
            }
        } else {
            return Err(anyhow!("Python process terminated unexpectedly during first fork"));
        }
    }
    
    // Sleep briefly to ensure first process completes
    std::thread::sleep(std::time::Duration::from_millis(500));
    
    // 5. Launch a second child process
    println!("Sending code to second child process...");
    writeln!(stdin, "FORK:print('Hello from child process 2')")
        .map_err(|e| anyhow!("Failed to write to stdin: {}", e))?;
    
    // Wait for response from the second fork
    fork_complete = false;
    while !fork_complete {
        if let Some(Ok(line)) = lines.next() {
            println!("Python process: {}", line);
            if line.starts_with("FORKED:") {
                // Successfully forked
                let fork_pid = line.trim_start_matches("FORKED:");
                println!("Forked second child process with PID: {}", fork_pid);
            } else if line.starts_with("FORK_RUN_COMPLETE:") {
                fork_complete = true;
            } else if line.starts_with("FORK_RUN_ERROR:") {
                return Err(anyhow!("Error in second child process: {}", 
                    line.trim_start_matches("FORK_RUN_ERROR:")));
            }
        } else {
            return Err(anyhow!("Python process terminated unexpectedly during second fork"));
        }
    }
    
    // 6. Clean up
    println!("Demo complete. Terminating Python process.");
    writeln!(stdin, "EXIT")
        .map_err(|e| anyhow!("Failed to write exit command: {}", e))?;
    
    child.wait()?;
    Ok(())
}*/
