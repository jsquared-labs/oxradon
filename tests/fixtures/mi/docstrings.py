"""A module docstring
over three lines.
"""
# A comment.
import os  # an inline comment


def f(x):
    """A docstring
    on two lines."""
    # c1
    # c2
    s = """not a docstring,
    but multi-line"""
    return x + len(s) + len(os.sep)


class C:
    """Class docstring."""

    def m(self):
        """Another
        one."""
        return 1
