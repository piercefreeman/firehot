"""
Test script for the hotreload Rust extension.
"""
from hotreload import sum_as_string

def test_sum_as_string() -> None:
    """Test the sum_as_string function from the Rust extension."""
    result = sum_as_string(5, 7)
    print(f"Result from Rust extension: {result}")
    assert result == "12"

if __name__ == "__main__":
    test_sum_as_string()
    print("All tests passed!") 