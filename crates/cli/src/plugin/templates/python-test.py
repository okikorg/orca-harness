from {{PACKAGE}}.server import echo


def test_echo_returns_its_input() -> None:
    assert echo("hello") == "hello"
