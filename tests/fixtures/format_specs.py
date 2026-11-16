"""Doubled braces in a format spec, after a nested field: text to CPython.

Ruff's lexer starts another field there, so the `if`, `or` and `for` below
would count as code. See `rawtok::prepass`.
"""


def literal_text(a, b, x, y, z):
    return f'{a:{b}{{x if y else z}}}'


def literal_then_code(a, b, p, q, x, y):
    return f'{a:{b}{{x or y}}}' + (p or q)


def unbalanced_text(a, b, x):
    if a:
        return f'{a:{b}{{x}'
    return t'{a:{b}{{x}'


def two_in_one(a, b, x, y):
    return f"{a:{b}{{x or y{{[i for i in x if i]}"


def nested_string(a, b, c, x, z):
    return f'{a:{f"{b:{c}{{z and x}"}{{x or z}'


def triple(a, b, c, d, p, q, x, y):
    return f'''{a:{b}
{{x or y}}}
{c:{d}{{p and q}}}'''


def really_code(a, b, x, y, z):
    return f'{a:{{x if y else z}}}' + f'{a:{b}{ {x or y} }}'
