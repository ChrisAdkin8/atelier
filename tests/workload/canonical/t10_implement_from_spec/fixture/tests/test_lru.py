import pytest

from lru import LRUCache


def test_basic_put_get():
    c = LRUCache(2)
    c.put("a", 1)
    assert c.get("a") == 1


def test_miss_returns_none():
    c = LRUCache(2)
    assert c.get("missing") is None


def test_eviction_at_capacity():
    c = LRUCache(2)
    c.put("a", 1)
    c.put("b", 2)
    c.put("c", 3)
    assert c.get("a") is None
    assert c.get("b") == 2
    assert c.get("c") == 3


def test_recently_used_promotion():
    c = LRUCache(2)
    c.put("a", 1)
    c.put("b", 2)
    assert c.get("a") == 1  # marks "a" as most-recently-used
    c.put("c", 3)            # should evict "b", not "a"
    assert c.get("a") == 1
    assert c.get("b") is None
    assert c.get("c") == 3


def test_put_existing_updates_value():
    c = LRUCache(2)
    c.put("a", 1)
    c.put("a", 2)
    assert c.get("a") == 2


def test_put_existing_promotes_recency():
    c = LRUCache(2)
    c.put("a", 1)
    c.put("b", 2)
    c.put("a", 99)   # "a" should now be most-recently-used
    c.put("c", 3)    # should evict "b", not "a"
    assert c.get("a") == 99
    assert c.get("b") is None
    assert c.get("c") == 3


def test_capacity_one():
    c = LRUCache(1)
    c.put("a", 1)
    c.put("b", 2)
    assert c.get("a") is None
    assert c.get("b") == 2
