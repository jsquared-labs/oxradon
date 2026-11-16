"""Tests for check_lexer.py's comparison: cases where it must report a problem.

Run with the oracle's Python (CPython 3.14.10), from the repository root:
PYTHON -m unittest discover -s scripts -p test_check_lexer.py
"""

import unittest

from check_lexer import POLICY, compare, lexer_verdict, message


def error(msg, line):
    return {"error": message((msg, line))}


class Compare(unittest.TestCase):
    def test_a_wrong_hal_error_is_a_problem(self):
        verdict = lexer_verdict("0b2\n")
        right = {"error": message(verdict)}
        problems, excused = compare(verdict, None, {
            "cc": (right, right),
            "hal": (right, error("invented", 999)),
        })
        self.assertFalse(excused)
        self.assertEqual([why for why, _, _ in problems], ["hal: the lexer's error"])

    def test_no_exemption_past_an_earlier_lexer_error(self):
        text = "if x:\n  x\n x\n\t\\\nx\n"
        verdict = lexer_verdict(text)
        self.assertEqual(verdict[1], 3)
        refusal = (5, error(POLICY, 5))
        right = {"error": message(verdict)}
        problems, excused = compare(verdict, refusal, {
            "cc": (right, refusal[1]),
            "hal": (right, refusal[1]),
        })
        self.assertFalse(excused)
        self.assertEqual([why for why, _, _ in problems], ["cc: the lexer's error", "hal: the lexer's error"])

    def test_the_row_is_excused_where_the_lexer_reaches_it(self):
        refusal = (2, error(POLICY, 2))
        problems, excused = compare(None, refusal, {
            "cc": ({"x": 1}, refusal[1]),
            "hal": ({"x": 1}, refusal[1]),
        })
        self.assertEqual((problems, excused), ([], True))


if __name__ == "__main__":
    unittest.main()
