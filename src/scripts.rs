// Embed Python scripts directly in the binary
pub const PYTHON_LOADER_SCRIPT: &str = include_str!("../firehot/embedded/parent_entrypoint.py");
pub const PYTHON_CHILD_SCRIPT: &str = include_str!("../firehot/embedded/child_entrypoint.py");
pub const PYTHON_CALL_SCRIPT: &str = include_str!("../firehot/embedded/call_serializer.py");
