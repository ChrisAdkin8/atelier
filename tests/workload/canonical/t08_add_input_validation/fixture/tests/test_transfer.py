import pytest

from transfer import ACCOUNTS, transfer


def setup_function():
    ACCOUNTS.clear()
    ACCOUNTS.update({"alice": 100, "bob": 50, "carol": 200})


def test_happy_path():
    a, b = transfer(20, "alice", "bob")
    assert a == 80
    assert b == 70
