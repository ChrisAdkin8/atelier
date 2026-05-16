from parse import parse


def api_render(payload):
    cfg = {"strict": payload.get("strict_mode", False)}
    return parse(payload["text"], cfg)
