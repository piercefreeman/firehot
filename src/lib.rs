use anyhow::{anyhow, Result};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    collections::{HashMap, HashSet},
    time::Duration,
};
use walkdir::WalkDir;
use once_cell::sync::Lazy;
use std::io::Write;

use rustpython_parser::{parse, Mode};
use rustpython_parser::ast::{
    Mod, Stmt,
    StmtIf, StmtWhile, StmtFunctionDef, StmtAsyncFunctionDef, StmtClassDef,
};

// Add PyO3 imports for Python bindings
use pyo3::prelude::*;
use pyo3::exceptions::PyRuntimeError;
use pyo3::types::PyDict;
use std::sync::{Arc, Mutex};
use uuid::Uuid;
use libc;

// Define our messages module
pub mod messages;
use messages::{ForkRequest, ExitRequest, Message};

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
const PYTHON_LOADER_SCRIPT: &str = include_str!("../python/parent_entrypoint.py");

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

// Replace single IMPORT_RUNNER with a registry of runners
static IMPORT_RUNNERS: Lazy<Mutex<HashMap<String, ImportRunner>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Runner for isolated Python code execution
struct ImportRunner {
    id: String,
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<std::process::ChildStdin>>,
    reader: Arc<Mutex<std::io::Lines<BufReader<std::process::ChildStdout>>>>,
    forked_processes: Arc<Mutex<HashMap<String, i32>>>, // Map of UUID to PID
}

impl ImportRunner {
    /// Execute a function in the isolated environment. This should be called from the main thread (the one
    /// that spawned our hotreloader) so we can get the local function and closure variables.
    fn exec_isolated(
        &self, 
        pickled_data: &str
    ) -> Result<String, String> {
        // Create the Python execution code that will unpickle and run the function
        // Errors don't seem to get caught in the parent process, so we need to log them here
        let exec_code = format!(
            r#"
import pickle
import base64
from traceback import format_exc

# Decode base64 and unpickle
pickled_bytes = base64.b64decode("{}")
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
            pickled_data
        );
        
        // Send the code to the forked process
        let mut stdin_guard = self.stdin.lock()
            .map_err(|e| format!("Failed to lock stdin mutex: {}", e))?;
        
        // Create a ForkRequest message
        let fork_request = Message::ForkRequest(ForkRequest::new(exec_code));
        let serialized = serde_json::to_string(&fork_request)
            .map_err(|e| format!("Failed to serialize message: {}", e))?;
        
            println!("Sending message: {}", serialized);
        writeln!(stdin_guard, "{}", serialized)
            .map_err(|e| format!("Failed to write to child process: {}", e))?;
        
        drop(stdin_guard);
        
        // Wait for response
        let mut reader_guard = self.reader.lock()
            .map_err(|e| format!("Failed to lock reader mutex: {}", e))?;

        for line in &mut *reader_guard {
            let line = line.map_err(|e| format!("Failed to read line: {}", e))?;
            
            // Parse the line as a message
            if let Ok(message) = serde_json::from_str::<messages::Message>(&line) {
                println!("Received message: {:?}", message);

                match message {
                    messages::Message::ForkResponse(response) => {
                        // Handle fork response message
                        let fork_pid = response.child_pid;
                        
                        // Generate a UUID for this process
                        let process_uuid = Uuid::new_v4().to_string();
                        
                        // Store the PID with its UUID
                        let mut forked_processes = self.forked_processes.lock()
                            .map_err(|e| format!("Failed to lock forked processes mutex: {}", e))?;
                        forked_processes.insert(process_uuid.clone(), fork_pid);
                        drop(forked_processes);
                        
                        // Wait a bit for the fork to complete to avoid race conditions
                        std::thread::sleep(Duration::from_millis(100));
                        
                        // Return the UUID
                        return Ok(process_uuid);
                    },
                    messages::Message::ChildError(error) => {
                        // Handle child error message
                        return Err(format!("Function execution failed: {}", error.error));
                    },
                    messages::Message::UnknownError(error) => {
                        // Handle unknown error message
                        return Err(format!("Process error: {}", error.error));
                    },
                    _ => {
                        // Log unhandled message types
                        println!("Unhandled message type: {:?}", message.name());
                        continue;
                    }
                }
            } else {
                // If we can't parse it as a message, log and continue
                println!("Received non-message line: {}", line);
            }
        }
        
        Err("Unexpected end of output from child process".to_string())
    }
    
    /// Stop an isolated process by UUID
    fn stop_isolated(&self, process_uuid: &str) -> Result<bool, String> {
        let mut forked_processes = self.forked_processes.lock()
            .map_err(|e| format!("Failed to lock forked processes mutex: {}", e))?;
            
        if let Some(pid) = forked_processes.get(process_uuid) {
            // Send EXIT_REQUEST message to the process
            let mut stdin_guard = self.stdin.lock()
                .map_err(|e| format!("Failed to lock stdin mutex: {}", e))?;
            
            // Create an ExitRequest message
            let exit_request = messages::ExitRequest::new();
            let serialized = serde_json::to_string(&exit_request)
                .map_err(|e| format!("Failed to serialize message: {}", e))?;
            writeln!(stdin_guard, "{}", serialized)
                .map_err(|e| format!("Failed to write exit request: {}", e))?;
            drop(stdin_guard);
            
            // Give the process a chance to exit gracefully
            std::thread::sleep(Duration::from_millis(100));
            
            // If it's still running, terminate it forcefully
            unsafe {
                // Use libc::kill to terminate the process
                if libc::kill(*pid, libc::SIGTERM) != 0 {
                    return Err(format!("Failed to terminate process: {}", std::io::Error::last_os_error()));
                }
            }
            
            // Remove the process from the map
            forked_processes.remove(process_uuid);
            Ok(true)
        } else {
            // This process doesn't own the UUID
            Ok(false)
        }
    }
    
    /// Communicate with an isolated process to get its output
    fn communicate_isolated(&self, process_uuid: &str) -> Result<Option<String>, String> {
        let forked_processes = self.forked_processes.lock()
            .map_err(|e| format!("Failed to lock forked processes mutex: {}", e))?;
            
        if !forked_processes.contains_key(process_uuid) {
            // This process doesn't own the UUID
            return Ok(None);
        }
        drop(forked_processes);
        
        // Read from the reader
        let mut reader_guard = self.reader.lock()
            .map_err(|e| format!("Failed to lock reader mutex: {}", e))?;
            
        // Check for messages from the process
        let mut output = String::new();
        for _ in 0..1000 { // Limit to avoid infinite loop
            println!("Reading line");
            match reader_guard.next() {
                Some(Ok(line)) => {
                    println!("Read line: {}", line);
                    // Parse the line as a message
                    if let Ok(message) = serde_json::from_str::<messages::Message>(&line) {
                        match message {
                            messages::Message::ChildComplete(complete) => {
                                // If we have a result, return it
                                if let Some(result) = complete.result {
                                    return Ok(Some(result));
                                }
                            },
                            messages::Message::ChildError(error) => {
                                // Return error message as output
                                return Ok(Some(format!("Error: {}", error.error)));
                            },
                            _ => {
                                // For other message types, add them to the output
                                let json = serde_json::to_string(&message)
                                    .map_err(|e| format!("Failed to serialize message: {}", e))?;
                                output.push_str(&json);
                                output.push('\n');
                            }
                        }
                    } else {
                        // Log unrecognized output but don't add it to the result
                        println!("Unrecognized output from process: {}", line);
                    }
                },
                Some(Err(e)) => return Err(format!("Error reading output: {}", e)),
                None => break, // No more output
            }
        }
        
        if output.is_empty() {
            Ok(None)
        } else {
            Ok(Some(output))
        }
    }
}

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
    
    let _package_name = package_name.ok_or_else(|| 
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
    
    // Wait for the ImportComplete message
    let mut imports_loaded = false;
    for line in &mut lines_iter {
        let line = line.map_err(|e| PyRuntimeError::new_err(format!("Failed to read line: {}", e)))?;
        
        // Parse the line as a message
        if let Ok(message) = serde_json::from_str::<Message>(&line) {
            match message {
                Message::ImportComplete(_) => {
                    imports_loaded = true;
                    break;
                },
                Message::ImportError(error) => {
                    return Err(PyRuntimeError::new_err(format!(
                        "Import error: {}", 
                        error.error
                    )));
                },
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
        return Err(PyRuntimeError::new_err("Python loader did not report successful imports"));
    }
    
    // Generate a unique identifier for this runner
    let runner_id = Uuid::new_v4().to_string();
    
    // Create the runner object
    let runner = ImportRunner {
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
        let mut child = runner.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        Ok(())
    } else {
        Err(PyRuntimeError::new_err(format!("No import runner found with ID: {}", runner_id)))
    }
}

/// Execute code with a specific runner
#[pyfunction]
fn exec_isolated<'py>(py: Python<'py>, runner_id: &str, func: PyObject, args: Option<PyObject>) -> PyResult<&'py PyAny> {
    // Create a dict to hold our function and args for pickling
    let locals = PyDict::new(py);
    locals.set_item("func", func)?;
    locals.set_item("args", args.unwrap_or_else(|| py.None()))?;
    
    // Pickle the function and args and encode in base64
    let pickle_code = "import pickle, base64; pickled_data = base64.b64encode(pickle.dumps((func, args))).decode('utf-8')";
    py.run(pickle_code, None, Some(locals))?;
    
    // Get the pickled data - now it's a string because we decoded it in Python
    let pickled_data = locals.get_item("pickled_data")
        .ok_or_else(|| PyRuntimeError::new_err("Failed to pickle function and args"))?
        .extract::<String>()?;

    let runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.get(runner_id) {
        // Convert Rust Result<String, String> to PyResult
        match runner.exec_isolated(&pickled_data) {
            Ok(result) => Ok(py.eval(&format!("'{}'", result), None, None)?),
            Err(err) => Err(PyRuntimeError::new_err(err))
        }
    } else {
        Err(PyRuntimeError::new_err(format!("No import runner found with ID: {}", runner_id)))
    }
}

/// Stop an isolated process by ID
#[pyfunction]
fn stop_isolated(_py: Python, runner_id: &str, process_uuid: &str) -> PyResult<bool> {
    let runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.get(runner_id) {
        runner.stop_isolated(process_uuid)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to stop isolated process: {}", e)))
    } else {
        Err(PyRuntimeError::new_err(format!("No import runner found with ID: {}", runner_id)))
    }
}

/// Communicate with an isolated process to get its output
#[pyfunction]
fn communicate_isolated(_py: Python, runner_id: &str, process_uuid: &str) -> PyResult<Option<String>> {
    let runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.get(runner_id) {
        runner.communicate_isolated(process_uuid)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to communicate with isolated process: {}", e)))
    } else {
        Err(PyRuntimeError::new_err(format!("No import runner found with ID: {}", runner_id)))
    }
}
