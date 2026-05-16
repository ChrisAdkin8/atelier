from parse import parse


def render_admin_label(name):
    return parse(name, {"strict": True, "upper": True})
