from external_package.mock_imports import external_function
from mypackage.dep import local_function

def main():
    external_function()
    local_function()

if __name__ == "__main__":
    main()
