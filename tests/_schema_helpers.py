"""Shared helpers for schema validation across the rig.

Builds a `referencing.Registry` mapping every schema's `$id` URL to its local
file contents, so cross-schema `$ref`s (e.g., `session.envelope -> model_protocol/envelope`)
resolve without network access.

Used by both `tests/validate_artifacts.py` and `tests/test_schemas.py`.
"""
import json
from pathlib import Path

import jsonschema
from referencing import Registry, Resource

ROOT = Path(__file__).resolve().parent.parent
SCHEMAS_DIR = ROOT / "schemas"


def build_schema_registry() -> Registry:
    """Build a registry mapping each schema's `$id` to its local-file content.

    Iterates `schemas/**.json`; any file declaring `$id` is added to the registry.
    """
    registry = Registry()
    for path in SCHEMAS_DIR.rglob("*.json"):
        schema = json.loads(path.read_text())
        sid = schema.get("$id")
        if sid:
            registry = registry.with_resource(uri=sid, resource=Resource.from_contents(schema))
    return registry


def validator_for(schema: dict, registry: Registry | None = None):
    """Return a configured `jsonschema` validator for `schema`, with cross-schema $refs wired.

    If `registry` is None, a fresh registry is built. Pass a pre-built registry when
    constructing many validators in a loop (avoids re-reading every schema file).
    """
    if registry is None:
        registry = build_schema_registry()
    ValidatorCls = jsonschema.validators.validator_for(schema)
    return ValidatorCls(schema, registry=registry)
