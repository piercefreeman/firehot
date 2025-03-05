from time import sleep

# Simulate a long import chain pipeline. This can sometimes happen
# with heavy dependencies.
sleep(4)

def some_function():
    print("some_function")
    sleep(4)
    print("some_function done")
