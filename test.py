import pickle
import base64

# Decode base64 and unpickle
pickled_bytes = base64.b64decode("gASVPQAAAAAAAACMCF9fbWFpbl9flIwJZ2xvYmFsX2ZulJOUjBpIZWxsbyBmcm9tIHRlc3RfaG90cmVsb2FkIZRLA4aUhpQu")
data = pickle.loads(pickled_bytes)
func, args = data

# Run the function with args
if isinstance(args, tuple):
    result = func(*args)
elif args is not None:
    result = func(args)
else:
    result = func()

print("RAN RESULT")


