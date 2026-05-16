from orders.cart import compute_total


def apply_discount(items, discount_pct):
    base = compute_total(items)
    return base * (1 - discount_pct / 100)
