use anyhow::{anyhow, Result};
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};
use walkdir::WalkDir;

use rustpython_parser::{parse, Mode};
use rustpython_parser::ast::{
    Mod, Stmt,
    StmtIf, StmtWhile, StmtFunctionDef, StmtAsyncFunctionDef, StmtClassDef,
};

/// A simple structure to hold information about an import.
#[derive(Debug)]
struct ImportInfo {
    /// For an `import X`, this is "X". For a `from X import Y`, this is "X".
    module: String,
    /// The names imported from that module.
    names: Vec<String>,
}

/// Recursively traverse AST statements to collect import information.
/// We treat absolute (level 0) imports as third-party imports.
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
                    });
                }
            }
            Stmt::ImportFrom(import_from) => {
                // Compare the level, which is Option<Int>; assume missing means 0.
                if import_from.level.unwrap_or(0) != 0 {
                    continue; // skip relative imports
                }
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

/// Given a path, scan for all Python files, parse them and extract the set of
/// absolute (non-relative) modules that are imported.
fn process_py_files(path: &Path) -> Result<HashSet<String>> {
    let mut third_party_modules = HashSet::new();

    // Walk the directory recursively.
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
        // Match on Mod::Module by reference so that we get a slice of statements.
        let stmts: &[Stmt] = match &parsed {
            Mod::Module(stmts) => stmts,
            _ => {
                return Err(anyhow!(
                    "Unexpected AST format for module in file {}",
                    file_path.display()
                ))
            }
        };
        let imports = collect_imports(stmts);
        for imp in imports {
            third_party_modules.insert(imp.module);
        }
    }
    Ok(third_party_modules)
}

/// Spawn a Python process that imports the given modules and then sleeps indefinitely.
/// The Python process prints "IMPORTS_LOADED" to stdout once it has finished importing.
fn spawn_python_loader(modules: &HashSet<String>) -> Result<Child> {
    let mut import_lines = String::new();
    for module in modules {
        import_lines.push_str(&format!(
            "try:\n    __import__('{}')\nexcept ImportError as e:\n    print('Failed to import {}:', e)\n",
            module, module
        ));
    }
    let python_script = format!(
        r#"
import time
import sys
{}
print("IMPORTS_LOADED", flush=True)
time.sleep(1000)
"#,
        import_lines
    );
    let child = Command::new("python")
        .args(&["-c", &python_script])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn python process: {}", e))?;
    Ok(child)
}

/// Take a memory snapshot of the process with the given PID and save it to dump_dir.
/// On Linux, CRIU is used; on macOS, gcore is used.
fn take_memory_snapshot(pid: u32, dump_dir: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        std::fs::create_dir_all(dump_dir)?;
        let status = Command::new("criu")
            .args(&[
                "dump",
                "-t",
                &pid.to_string(),
                "-D",
                dump_dir.to_str().unwrap(),
                "--shell-job",
                "--leave-running",
            ])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow!("CRIU dump failed"))
        }
    }
    #[cfg(target_os = "macos")]
    {
        std::fs::create_dir_all(dump_dir)?;
        let snapshot_file = dump_dir.join(format!("core.{}", pid));
        let status = Command::new("gcore")
            .args(&["-o", snapshot_file.to_str().unwrap(), &pid.to_string()])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow!("gcore snapshot failed"))
        }
    }
}

/// Main function tying all steps together.
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        return Err(anyhow!("Usage: {} <path_to_scan>", args[0]));
    }
    let scan_path = PathBuf::from(&args[1]);

    // 1. Process Python files.
    let modules = process_py_files(&scan_path)?;
    println!("Found third-party modules to load: {:?}", modules);

    // 2. Spawn a separate Python process to load these modules.
    let mut child = spawn_python_loader(&modules)?;

    // 3. Read stdout until we see "IMPORTS_LOADED".
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = line?;
            println!("Python process: {}", line);
            if line.trim() == "IMPORTS_LOADED" {
                break;
            }
        }
    } else {
        return Err(anyhow!("Failed to capture stdout from python process"));
    }

    // 4. Take a memory snapshot of the running Python process.
    let pid = child.id();
    let dump_dir = Path::new("./python_memory_snapshot");
    println!("Taking memory snapshot of process {} into {:?}", pid, dump_dir);
    take_memory_snapshot(pid, dump_dir)?;

    println!("Memory snapshot successfully saved.");
    child.kill()?;
    Ok(())
}
