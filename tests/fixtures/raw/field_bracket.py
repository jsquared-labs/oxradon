# A field closed by the wrong bracket. Python's `tokenize` accepted this until CPython 3.14.10,
# so radon's `raw` counted the file; since then it is an error.
a = 1
b = f"{a)(}"
c = 2
