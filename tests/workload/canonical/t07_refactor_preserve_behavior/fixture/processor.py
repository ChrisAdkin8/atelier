"""A long function mixing validation, transformation, aggregation, and formatting."""


def process(raw_data):
    if not isinstance(raw_data, list):
        raise TypeError("expected list")
    cleaned = []
    for item in raw_data:
        if not isinstance(item, dict):
            continue
        if "id" not in item or "value" not in item:
            continue
        if not isinstance(item["value"], (int, float)):
            continue
        if isinstance(item["value"], bool):
            continue
        cleaned.append(item)

    transformed = []
    for item in cleaned:
        new = {"id": item["id"], "value": item["value"] * 2}
        if item["value"] < 0:
            new["flag"] = "negative"
        elif item["value"] > 100:
            new["flag"] = "large"
        transformed.append(new)

    total = sum(item["value"] for item in transformed)
    count = len(transformed)
    average = total / count if count else 0

    return {
        "items": transformed,
        "stats": {"total": total, "count": count, "average": average},
    }
