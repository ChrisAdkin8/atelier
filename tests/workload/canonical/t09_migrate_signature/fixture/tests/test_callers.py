import pytest

from admin import render_admin_label
from api import api_render
from batch import render_batch
from user import render_username


def test_user_render():
    assert render_username("alice") == "ALICE"


def test_admin_render():
    assert render_admin_label("alice") == "ALICE"


def test_admin_strict_rejects_empty():
    with pytest.raises(ValueError):
        render_admin_label("")


def test_batch_render():
    assert render_batch(["one", "two", "three"]) == ["one", "two", "three"]


def test_api_render():
    assert api_render({"text": "hi"}) == "hi"


def test_api_strict_rejects_empty():
    with pytest.raises(ValueError):
        api_render({"text": "", "strict_mode": True})
