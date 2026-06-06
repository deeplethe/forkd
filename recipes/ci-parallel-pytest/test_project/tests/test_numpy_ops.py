"""numpy array tests — the import alone is ~150 ms cold.

The whole point of forkd for CI: numpy is already imported in the
parent snapshot, so children inherit the import for free. Each
child's pytest startup skips the import cost.
"""
import numpy as np
import pytest


def test_zeros_shape() -> None:
    a = np.zeros((4, 5))
    assert a.shape == (4, 5)
    assert a.sum() == 0


def test_linspace_endpoints() -> None:
    a = np.linspace(0.0, 1.0, 11)
    assert a[0] == pytest.approx(0.0)
    assert a[-1] == pytest.approx(1.0)
    assert len(a) == 11


def test_dot_product_associative() -> None:
    rng = np.random.default_rng(seed=0)
    a = rng.standard_normal((3, 3))
    b = rng.standard_normal((3, 3))
    c = rng.standard_normal((3, 3))
    np.testing.assert_allclose((a @ b) @ c, a @ (b @ c), rtol=1e-10)


def test_solve_smoke() -> None:
    a = np.array([[2.0, 1.0], [1.0, 3.0]])
    b = np.array([1.0, 2.0])
    x = np.linalg.solve(a, b)
    np.testing.assert_allclose(a @ x, b)


def test_eigvals_real_for_symmetric() -> None:
    rng = np.random.default_rng(seed=1)
    a = rng.standard_normal((8, 8))
    sym = (a + a.T) / 2
    vals = np.linalg.eigvalsh(sym)
    assert np.allclose(vals.imag, 0.0)
    assert len(vals) == 8


def test_fft_inverse_round_trips() -> None:
    rng = np.random.default_rng(seed=2)
    a = rng.standard_normal(64)
    recovered = np.fft.ifft(np.fft.fft(a)).real
    np.testing.assert_allclose(a, recovered, atol=1e-10)
