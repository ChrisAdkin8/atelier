def parse(input, opts={}):
    """Parse `input` per `opts`."""
    if opts.get("strict") and not input:
        raise ValueError("strict: empty input")
    return input.upper() if opts.get("upper") else input
