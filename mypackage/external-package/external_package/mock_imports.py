from datetime import datetime
from time import sleep

# Simulate a long import chain pipeline. This can sometimes happen
# with heavy dependencies.
print("Loading heavy external package import", datetime.now())
sleep(4)
print("Heavy external package import done", datetime.now())


def external_function():
    print("external_function")
    print("external_function done")
