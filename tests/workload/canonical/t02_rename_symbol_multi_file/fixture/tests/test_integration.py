from orders.cart import compute_total
from orders.checkout import checkout
from orders.discount import apply_discount


def test_full_flow():
    items = [{"price": 10, "qty": 2}]
    total = compute_total(items)
    assert total == 20
    discounted = apply_discount(items, 10)
    assert discounted == 18
    result = checkout(items, "cash")
    assert result["total"] == 20
