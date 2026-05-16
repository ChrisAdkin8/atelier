The function `process` in `processor.py` mixes four concerns: validation, transformation, aggregation, and formatting. Refactor it into a top-level `process` function plus at least three helper functions, each handling a single concern.

Constraints:
- All existing tests must still pass.
- Public API is unchanged: callers continue to call `process(raw_data)` with the same input and the same return shape.
- The top-level `process` body should be a short composition of the helpers.

Do not change the test file.
