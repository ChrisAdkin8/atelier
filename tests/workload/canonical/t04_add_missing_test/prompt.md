`iso.py` contains a `parse_iso_date(s)` function but has no tests. Write a test suite in `tests/test_iso.py` covering at minimum:

- A valid date round-trips through `parse_iso_date` and returns a `datetime.date`.
- An invalid format (e.g., `"2024/01/15"`, `"not-a-date"`) raises `ValueError`.
- A leap-year boundary case (Feb 29 of a leap year).
- A month-boundary case (e.g., March 31 followed by April 1).

`pytest fixture/` must pass and report at least 4 collected tests.
