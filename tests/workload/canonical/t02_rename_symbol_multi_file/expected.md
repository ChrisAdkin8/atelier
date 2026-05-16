# t02 — Expected outcome

## Mechanical checks
- `grep -r "compute_total" fixture/` returns no matches
- `pytest fixture/` exits 0
- `python -c "import sys; sys.path.insert(0, 'fixture'); from orders.cart import compute_grand_total; assert compute_grand_total([{'price': 3, 'qty': 2}]) == 6"` exits 0

## Files that must change
- `fixture/orders/cart.py` — definition renamed
- `fixture/orders/checkout.py` — import + call site
- `fixture/orders/receipt.py` — import + call site
- `fixture/orders/discount.py` — import + call site
- `fixture/orders/api.py` — import + call site
- `fixture/tests/test_cart.py` — import + 3 call sites
- `fixture/tests/test_checkout.py` — import + 1 call site
- `fixture/tests/test_integration.py` — import + 1 call site
- `fixture/README.md` — every reference to `compute_total` updated

## Invariants
- Behavior preserved: every existing test still passes after rename
- No file added or removed
- No file modified that does not currently contain `compute_total`

## Permission-prompt expectations
- Reasonable upper bound on tool calls: 9 reads + 9 writes + 1 test run = **19 actions**
- Baseline (current Claude Code with default settings): record actual on `tests/baselines/permission_prompts.json`
- Atelier target: with §8 learning, after the first write in `fixture/orders/` is approved, subsequent writes in same shape auto-approve. Same for `fixture/tests/`. Expected ≤4 permission prompts total (one per write-shape × 2 directories, one for grep, one for test run).

## Turn-budget
- Hard cap: 20 turns
- Expected median: 3–4 turns (grep to find call sites, batch edit, run tests, fix any miss)
