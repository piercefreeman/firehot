from external_package.mock_imports import some_function
from hotreload.dep import local_function

def main():
    some_function()
    local_function()

if __name__ == "__main__":
    main()
