from orders.cart import compute_total


def test_compute_total_empty():
    assert compute_total([]) == 0


def test_compute_total_single():
    assert compute_total([{"price": 5, "qty": 2}]) == 10


def test_compute_total_multi():
    items = [{"price": 3, "qty": 2}, {"price": 4, "qty": 1}]
    assert compute_total(items) == 10
