"""Tiny demo CLI."""
import argparse


def build_parser():
    parser = argparse.ArgumentParser(prog="mycli")
    parser.add_argument("name", help="Name to greet")
    parser.add_argument("--greeting", default="Hello", help="Greeting word")
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    return f"{args.greeting}, {args.name}!"


if __name__ == "__main__":
    print(main())
