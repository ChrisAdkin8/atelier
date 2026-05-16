from parse import parse


def render_batch(names):
    return [parse(n) for n in names]
