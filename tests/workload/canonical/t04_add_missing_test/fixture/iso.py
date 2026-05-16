"""ISO date parsing."""
import re
from datetime import date

_PATTERN = re.compile(r"^(\d{4})-(\d{2})-(\d{2})$")


def parse_iso_date(s):
    """Parse 'YYYY-MM-DD' into a `datetime.date`. Raise `ValueError` on invalid input."""
    m = _PATTERN.match(s)
    if not m:
        raise ValueError(f"not an ISO date: {s!r}")
    y, mo, d = map(int, m.groups())
    return date(y, mo, d)
