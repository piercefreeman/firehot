"""
Test script for the import analyzer and memory snapshot functionality.
"""
from hotreload import analyze_and_snapshot_imports
import os
import json
from typing import Dict, List, Any, Optional

def test_analyze_imports(
    directory_path: str, 
    output_path: Optional[str] = None,
    exclude_dirs: Optional[List[str]] = None
) -> Dict[str, Any]:
    """
    Test the analyze_and_snapshot_imports function from the Rust extension.
    
    Args:
        directory_path: Path to the directory containing Python files to analyze
        output_path: Optional path where the memory snapshot will be saved
        exclude_dirs: Optional list of directory names to exclude from analysis
        
    Returns:
        Dictionary with analysis results
    """
    # Call the Rust function to analyze imports and take memory snapshot
    result = analyze_and_snapshot_imports(
        directory_path,
        output_path,
        exclude_dirs
    )
    
    print(f"Analysis complete!")
    print(f"Found {len(result['imports'])} third-party imports:")
    for imp in result['imports']:
        print(f"  - {imp}")
    
    print(f"\nAnalyzed {len(result['analyzed_files'])} Python files")
    
    print(f"\nMemory snapshot saved to: {result['snapshot_path']}")
    
    # Load and display some information from the snapshot
    if os.path.exists(result['snapshot_path']):
        with open(result['snapshot_path'], 'r') as f:
            snapshot_data = json.load(f)
        
        print(f"\nSnapshot details:")
        print(f"  - Successfully loaded modules: {len(snapshot_data['successfully_loaded'])}")
        print(f"  - Failed imports: {len(snapshot_data['failed_imports'])}")
        print(f"  - Total memory usage: {snapshot_data['total_memory'] / (1024 * 1024):.2f} MB")
        
        if snapshot_data['memory_stats']:
            print(f"\nTop 5 memory consumers:")
            for i, stat in enumerate(snapshot_data['memory_stats'][:5]):
                print(f"  {i+1}. {stat['file']} (line {stat['line']}): {stat['size'] / 1024:.2f} KB ({stat['count']} objects)")
    
    return result

if __name__ == "__main__":
    # Example usage - analyze the current directory
    current_dir = os.path.dirname(os.path.abspath(__file__))
    test_analyze_imports(current_dir) 