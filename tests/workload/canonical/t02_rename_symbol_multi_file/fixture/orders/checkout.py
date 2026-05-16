from orders.cart import compute_total


def checkout(items, payment_method):
    total = compute_total(items)
    return {"total": total, "paid_via": payment_method}
