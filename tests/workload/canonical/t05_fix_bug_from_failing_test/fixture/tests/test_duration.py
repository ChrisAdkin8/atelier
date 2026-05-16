from duration import format_duration


def test_under_hour():
    assert format_duration(1500) == "25m"


def test_combined():
    assert format_duration(5400) == "1h30m"


def test_exact_hour():
    assert format_duration(7200) == "2h"


def test_zero():
    assert format_duration(0) == "0m"


def test_just_under_an_hour():
    assert format_duration(3599) == "59m"
