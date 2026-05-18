"""Three concerns smushed into one function: parsing, scoring, reporting."""


def pipeline(raw):
    rows = []
    for line in raw.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split(",")
        if len(parts) != 2:
            continue
        try:
            rows.append({"name": parts[0].strip(), "score": int(parts[1])})
        except ValueError:
            continue

    for r in rows:
        s = r["score"]
        if s >= 90:
            r["grade"] = "A"
        elif s >= 80:
            r["grade"] = "B"
        elif s >= 70:
            r["grade"] = "C"
        else:
            r["grade"] = "F"

    lines = [f"{r['name']}: {r['score']} ({r['grade']})" for r in rows]
    return {"rows": rows, "report": "\n".join(lines)}
