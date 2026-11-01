"""Prints two paths that start with argv[1] and end in a number, and whose FNV-1a hash cut to 32 bits is equal.

The old unix socket name was that cut hash of BATON_HOME, so e2e/unix.sh uses the pair to show two homes no
longer share a daemon.
"""

import sys

base = sys.argv[1]


def fnv32(data):
    h = 0xCBF29CE484222325
    for c in data:
        h = ((h ^ c) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h & 0xFFFFFFFF


seen = {}
for i in range(5_000_000):
    path = base + str(i)
    key = fnv32(path.encode())
    if key in seen:
        print(seen[key])
        print(path)
        break
    seen[key] = path
