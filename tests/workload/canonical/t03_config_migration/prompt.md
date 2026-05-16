The config schema is being migrated from "legacy" key names to clean ones. Two of the existing tests pass; one fails because the new-schema test cannot find the new keys.

Update `config.json` so it uses the new schema:
- `legacy_name` → `name`
- `legacy_timeout_seconds` → `timeout_seconds`

Update `config.py` to read the new keys. Make all tests pass.
