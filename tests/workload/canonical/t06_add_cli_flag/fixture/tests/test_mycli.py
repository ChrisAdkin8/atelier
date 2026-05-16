from mycli import main, build_parser


def test_default_greeting():
    assert main(["World"]) == "Hello, World!"


def test_custom_greeting():
    assert main(["--greeting", "Hi", "Bob"]) == "Hi, Bob!"


def test_help_contains_name():
    help_text = build_parser().format_help()
    assert "name" in help_text
