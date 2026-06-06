"""Tiny arithmetic suite — exercises pytest startup + import overhead.

Realistic CI suites have hundreds of these; they're individually cheap
but the per-test fixed overhead (`pytest` startup + test collection +
fixture setup) is what eats wall-clock when run sequentially.
"""
import pytest


@pytest.mark.parametrize("a,b,expected", [(1, 2, 3), (5, 7, 12), (-1, 1, 0), (0, 0, 0)])
def test_addition(a: int, b: int, expected: int) -> None:
    assert a + b == expected


@pytest.mark.parametrize("a,b,expected", [(10, 3, 30), (-2, 4, -8), (0, 999, 0)])
def test_multiplication(a: int, b: int, expected: int) -> None:
    assert a * b == expected


def test_division_by_zero_raises() -> None:
    with pytest.raises(ZeroDivisionError):
        _ = 1 / 0


def test_modulo_invariant() -> None:
    for n in range(20):
        assert n % 7 in range(7)
