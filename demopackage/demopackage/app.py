from external_package.mock_imports import external_function

from demopackage.dep import local_function


def run_everything():
    external_function()
    local_function()
