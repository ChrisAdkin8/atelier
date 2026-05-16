from orders.cart import compute_total


def render_receipt(items):
    total = compute_total(items)
    lines = [f"{item['name']}: {item.get('price', 0)}" for item in items]
    lines.append(f"Total: {total}")
    return "\n".join(lines)
