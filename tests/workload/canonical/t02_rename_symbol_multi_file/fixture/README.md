# Orders package

A minimal ordering module. The central function is `compute_total(items)`, which sums `price * qty` across line items. Downstream helpers:

- `checkout(items, payment_method)` — wraps `compute_total` and records the payment method.
- `apply_discount(items, discount_pct)` — applies a percentage discount to `compute_total`.
- `render_receipt(items)` — text output including `compute_total`.
- `order_summary(items)` — dict containing `compute_total` and item count.
