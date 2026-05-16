"""Cart total computation."""


def compute_total(items):
    """Return the sum of price * qty for each item."""
    return sum(item.get("price", 0) * item.get("qty", 1) for item in items)
