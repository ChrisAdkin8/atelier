from parse import parse


def render_username(name):
    return parse(name, {"upper": True})
