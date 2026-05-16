import json
from pathlib import Path

from config import load_config


def test_environment():
    cfg = load_config()
    assert cfg["environment"] == "production"


def test_timeout():
    cfg = load_config()
    assert cfg["timeout"] == 30


def test_uses_new_schema():
    data = json.loads((Path(__file__).parent.parent / "config.json").read_text())
    assert "name" in data
    assert "timeout_seconds" in data
    assert "legacy_name" not in data
    assert "legacy_timeout_seconds" not in data
