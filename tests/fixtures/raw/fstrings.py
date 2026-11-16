a = f"{x!r:>{w}}b"
b = f"{{literal}} {x}"
c = f"\N{BULLET} {x}"
d = f"""multi {
    x  # a comment inside a replacement field
} line"""
e = f"""{x:
}"""
g = f"{f'{y}'}"
h = t"{x} and {y!s}"
i = rf"\{x}"
j = f"{x:{y:{z}}}"
k = f"a" f"b"
