"""
Test the pretty print import error functionality
"""

import os
import tempfile

from firehot.embedded.parent_entrypoint import pretty_print_import_error


def test_pretty_print_import_error():
    # Create a temporary file with some import statements
    with tempfile.NamedTemporaryFile(mode="w", suffix=".py", delete=False) as f:
        f.write("""import os
import sys
import nonexistent_module  # This will fail

def some_function():
    import another_bad_module
    pass
""")
        temp_file = f.name

    try:
        # Create module locations data
        module_locations = {
            "nonexistent_module": [
                {
                    "file_path": temp_file,
                    "line": 3,
                    "column": 1,
                    "end_line": None,
                    "end_column": None,
                }
            ],
            "another_bad_module": [
                {
                    "file_path": temp_file,
                    "line": 6,
                    "column": 5,
                    "end_line": None,
                    "end_column": None,
                }
            ],
        }

        # Test with a module that has location info
        error = ImportError("No module named 'nonexistent_module'")
        result = pretty_print_import_error(error, "nonexistent_module", module_locations)

        print("Pretty printed error:")
        print(result)

        # Verify the output contains expected elements
        assert "ImportError: No module named 'nonexistent_module'" in result
        assert f'File "{temp_file}", line 3' in result
        assert "import nonexistent_module" in result
        assert "> " in result  # The line indicator

        # Test with a module that doesn't have location info
        error2 = ImportError("No module named 'unknown_module'")
        result2 = pretty_print_import_error(error2, "unknown_module", module_locations)

        print("\nError for module without location info:")
        print(result2)

        assert "ImportError: No module named 'unknown_module'" in result2
        assert "File" not in result2  # Should not contain file info

    finally:
        # Clean up
        os.unlink(temp_file)
