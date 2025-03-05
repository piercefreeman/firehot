from time import sleep
from datetime import datetime

# Simulate a long import chain pipeline. This can sometimes happen
# with heavy dependencies.
print("Loading heavy external package import", datetime.now())
#sleep(4)
print("Heavy external package import done", datetime.now())

def external_function():
    print("some_function")
    sleep(4)
    print("some_function done")
