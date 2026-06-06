"""sklearn model tests — `import sklearn` is ~600-1200 ms cold.

This is the worst per-test fixed cost in a typical Python ML CI:
every fresh pytest invocation re-pays it. In a forkd parent, the
import is part of the warmed snapshot.
"""
import numpy as np
import pytest
from sklearn.cluster import KMeans
from sklearn.datasets import make_classification
from sklearn.linear_model import LinearRegression, LogisticRegression
from sklearn.metrics import accuracy_score, r2_score
from sklearn.model_selection import train_test_split


def test_linear_regression_exact_fit() -> None:
    rng = np.random.default_rng(seed=0)
    x = rng.standard_normal((100, 3))
    coef_true = np.array([1.5, -2.0, 0.5])
    y = x @ coef_true + 7.0
    model = LinearRegression().fit(x, y)
    np.testing.assert_allclose(model.coef_, coef_true, atol=1e-9)
    assert model.intercept_ == pytest.approx(7.0)
    assert r2_score(y, model.predict(x)) == pytest.approx(1.0)


def test_logistic_regression_separable() -> None:
    x, y = make_classification(
        n_samples=200, n_features=8, n_informative=4, random_state=42,
    )
    x_tr, x_te, y_tr, y_te = train_test_split(x, y, test_size=0.25, random_state=0)
    model = LogisticRegression(max_iter=500).fit(x_tr, y_tr)
    acc = accuracy_score(y_te, model.predict(x_te))
    assert acc > 0.75


def test_kmeans_two_clusters() -> None:
    rng = np.random.default_rng(seed=7)
    cluster_a = rng.standard_normal((50, 2)) + [0, 0]
    cluster_b = rng.standard_normal((50, 2)) + [10, 10]
    x = np.vstack([cluster_a, cluster_b])
    km = KMeans(n_clusters=2, random_state=0, n_init=10).fit(x)
    centers = sorted(km.cluster_centers_.tolist(), key=lambda c: c[0])
    assert centers[0][0] < 2.0
    assert centers[1][0] > 8.0


def test_pipeline_predict_shape() -> None:
    rng = np.random.default_rng(seed=3)
    x = rng.standard_normal((50, 4))
    y = x @ np.array([1.0, -1.0, 0.5, 0.0]) + 2.0
    pred = LinearRegression().fit(x, y).predict(x[:10])
    assert pred.shape == (10,)
