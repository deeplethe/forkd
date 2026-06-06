"""String / regex tests — stdlib-only, very fast per test.

The point of including these is to mirror real CI suites: a mix of
heavy ML tests and fast unit tests. forkd's per-worker fixed cost
is amortized across whatever slice each worker gets.
"""
import re

import pytest


@pytest.mark.parametrize("s,expected", [
    ("hello world", 11),
    ("forkd", 5),
    ("", 0),
])
def test_string_length(s: str, expected: int) -> None:
    assert len(s) == expected


def test_split_join_roundtrip() -> None:
    s = "one two three four"
    assert " ".join(s.split()) == s


def test_regex_email_basic() -> None:
    pattern = re.compile(r"^[\w.+-]+@[\w.-]+\.[a-z]{2,}$", re.IGNORECASE)
    assert pattern.match("alice@example.com")
    assert pattern.match("user+tag@sub.example.co.uk")
    assert not pattern.match("not-an-email")
    assert not pattern.match("@example.com")


def test_dict_comprehension() -> None:
    src = {"a": 1, "b": 2, "c": 3}
    doubled = {k: v * 2 for k, v in src.items()}
    assert doubled == {"a": 2, "b": 4, "c": 6}


def test_set_operations() -> None:
    a = {1, 2, 3, 4}
    b = {3, 4, 5, 6}
    assert a & b == {3, 4}
    assert a | b == {1, 2, 3, 4, 5, 6}
    assert a - b == {1, 2}
