"""A second file that imports `greet` from sample.py.

Opening this alongside sample.py lets us watch a single rename produce edits
in *two* files at once — the cross-file case any real refactoring tool hits
constantly. ty resolves the import via interFileDependencies.
"""

from sample import greet

reply = greet("world")
print(reply)
