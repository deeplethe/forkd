"""pandas DataFrame tests — `import pandas` is ~400-800 ms cold."""
import numpy as np
import pandas as pd
import pytest


@pytest.fixture
def sample_df() -> pd.DataFrame:
    return pd.DataFrame({
        "user": ["alice", "bob", "carol", "dave", "eve"],
        "age": [29, 41, 35, 22, 58],
        "score": [0.81, 0.65, 0.92, 0.74, 0.88],
    })


def test_dataframe_construction(sample_df: pd.DataFrame) -> None:
    assert len(sample_df) == 5
    assert list(sample_df.columns) == ["user", "age", "score"]


def test_filter_by_age(sample_df: pd.DataFrame) -> None:
    adults = sample_df[sample_df["age"] >= 30]
    assert len(adults) == 3
    assert set(adults["user"]) == {"bob", "carol", "eve"}


def test_groupby_aggregation() -> None:
    df = pd.DataFrame({
        "team": ["a", "a", "b", "b", "c"],
        "score": [1, 2, 3, 4, 5],
    })
    means = df.groupby("team")["score"].mean()
    assert means["a"] == pytest.approx(1.5)
    assert means["b"] == pytest.approx(3.5)
    assert means["c"] == pytest.approx(5.0)


def test_merge_inner() -> None:
    left = pd.DataFrame({"id": [1, 2, 3], "x": ["a", "b", "c"]})
    right = pd.DataFrame({"id": [2, 3, 4], "y": [20, 30, 40]})
    merged = left.merge(right, on="id", how="inner")
    assert len(merged) == 2
    assert list(merged["y"]) == [20, 30]


def test_to_csv_roundtrip(tmp_path) -> None:
    df = pd.DataFrame({"a": [1, 2, 3], "b": [4, 5, 6]})
    p = tmp_path / "x.csv"
    df.to_csv(p, index=False)
    loaded = pd.read_csv(p)
    pd.testing.assert_frame_equal(df, loaded)


def test_numeric_describe(sample_df: pd.DataFrame) -> None:
    stats = sample_df[["age", "score"]].describe()
    assert stats.loc["count", "age"] == 5
    assert stats.loc["min", "score"] == pytest.approx(0.65)
