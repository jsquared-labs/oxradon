# How CPython positions the pieces of f-strings and t-strings decides `endline`.


def empty_piece_starts_the_run():
    return (f"{x}"
            ""
            "tail")


def empty_format_spec():
    return f"""{x
    :}"""


def nested_spec_and_conversion(width):
    first = (f"{x!r:{width}}"
             f"{y=:}")
    return first if width else t"{z:>{width}}"


def template_strings(name):
    return (t"hello {name}"
            t""
            t"{name:}")


def escaped_newline_piece():
    return (f"\
"
            f"tail")
