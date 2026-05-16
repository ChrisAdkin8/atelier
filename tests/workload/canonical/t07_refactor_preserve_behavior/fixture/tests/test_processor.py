import pytest

from processor import process


def test_happy_path():
    result = process([{"id": 1, "value": 10}, {"id": 2, "value": 20}])
    assert result["items"] == [
        {"id": 1, "value": 20},
        {"id": 2, "value": 40},
    ]
    assert result["stats"] == {"total": 60, "count": 2, "average": 30}


def test_negative_flag():
    result = process([{"id": 1, "value": -5}])
    assert result["items"] == [{"id": 1, "value": -10, "flag": "negative"}]


def test_large_flag():
    result = process([{"id": 1, "value": 200}])
    assert result["items"] == [{"id": 1, "value": 400, "flag": "large"}]


def test_drops_invalid():
    result = process([
        {"id": 1, "value": 5},
        "garbage",
        {"id": 2},
        {"value": 3},
        {"id": 3, "value": "ten"},
    ])
    assert [it["id"] for it in result["items"]] == [1]


def test_empty():
    result = process([])
    assert result == {"items": [], "stats": {"total": 0, "count": 0, "average": 0}}


def test_rejects_non_list():
    with pytest.raises(TypeError):
        process({"not": "a list"})
