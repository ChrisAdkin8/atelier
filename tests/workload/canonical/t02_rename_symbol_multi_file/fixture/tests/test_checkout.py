from orders.cart import compute_total
from orders.checkout import checkout


def test_checkout_total_matches_cart():
    items = [{"price": 3, "qty": 1}]
    result = checkout(items, "card")
    assert result["total"] == compute_total(items)


def test_checkout_payment_method_recorded():
    result = checkout([{"price": 1, "qty": 1}], "cash")
    assert result["paid_via"] == "cash"
