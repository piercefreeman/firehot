
// Embed Python scripts directly in the binary
pub const PYTHON_LOADER_SCRIPT: &str = include_str!("../hotreload/embedded/parent_entrypoint.py");
pub const PYTHON_CHILD_SCRIPT: &str = include_str!("../hotreload/embedded/child_entrypoint.py");
pub const PYTHON_CALL_SCRIPT: &str = include_str!("../hotreload/embedded/call_serializer.py");
