// Embed Python scripts directly in the binary
pub const PYTHON_LOADER_SCRIPT: &str = include_str!("firehot/embedded/parent_entrypoint.py");
pub const PYTHON_CHILD_SCRIPT: &str = include_str!("firehot/embedded/child_entrypoint.py");
pub const PYTHON_CALL_SCRIPT: &str = include_str!("firehot/embedded/call_serializer.py");
pub const PYTHON_IMPORT_SAFETY_PROBE_SCRIPT: &str =
    include_str!("firehot/embedded/import_safety_probe.py");
