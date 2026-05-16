from orders.cart import compute_total


def order_summary(items):
    return {"total": compute_total(items), "count": len(items)}
