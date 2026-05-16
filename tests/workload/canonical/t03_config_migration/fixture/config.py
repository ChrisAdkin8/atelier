import json
from pathlib import Path


def load_config():
    """Load config.json, mapping legacy keys to clean field names."""
    data = json.loads((Path(__file__).parent / "config.json").read_text())
    return {
        "environment": data["legacy_name"],
        "timeout": data["legacy_timeout_seconds"],
    }
