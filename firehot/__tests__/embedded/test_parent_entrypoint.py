import logging
import sys
from time import sleep

from firehot.embedded.parent_entrypoint import MultiplexedStream


def test_print_redirection(capfd):
    """
    Test that print() output is redirected and prefixed with [PID:<pid>:stdout].
    """
    with MultiplexedStream.setup_stream_redirection():
        print("Hello stdout!")
        sys.stdout.flush()
        sleep(0.2)

    captured = capfd.readouterr()
    # Ensure that the printed text appears along with the redirection prefix.
    assert "Hello stdout!" in captured.out
    assert "[PID:" in captured.out
    assert ":stdout]" in captured.out


def test_stderr_redirection(capfd):
    """
    Test that writing directly to sys.stderr is redirected and annotated with the correct prefix.
    """
    with MultiplexedStream.setup_stream_redirection():
        sys.stderr.write("Hello stderr!\n")
        sys.stderr.flush()
        sleep(0.2)

    captured = capfd.readouterr()
    assert "Hello stderr!" in captured.err
    assert "[PID:" in captured.err
    assert ":stderr]" in captured.err


def test_logging_redirection(capfd):
    """
    Test that logging output is captured and prefixed as expected.
    Note: Logging output is normally sent to stderr.
    """
    logger = logging.getLogger("test_logging")
    logger.setLevel(logging.DEBUG)
    # Clear any existing handlers.
    logger.handlers.clear()

    # Create the logging handler inside the redirection context so it uses the new sys.stderr.
    with MultiplexedStream.setup_stream_redirection():
        handler = logging.StreamHandler(sys.stderr)
        logger.addHandler(handler)
        logger.info("Logging test message")
        sys.stderr.flush()
        sleep(0.2)
        logger.removeHandler(handler)

    captured = capfd.readouterr()
    combined_output = captured.out + captured.err
    assert "Logging test message" in combined_output
    assert "[PID:" in combined_output


def test_stream_restoration(capfd):
    """
    Test that after the context manager exits, sys.stdout and sys.stderr are restored,
    and output printed outside the context does not have the redirection prefix.
    """
    with MultiplexedStream.setup_stream_redirection():
        print("Inside redirection")
        sys.stdout.flush()
        sleep(0.2)
        # Flush the captured output so that only new output is tested.
        _ = capfd.readouterr()

    # After context exit, printing should produce unmodified output.
    print("Outside redirection")
    sys.stdout.flush()
    captured_outside = capfd.readouterr()
    assert "Outside redirection" in captured_outside.out
    # The redirection prefix should not be present in output printed after the context.
    assert "[PID:" not in captured_outside.out


def test_multiline_print_prefix(capfd):
    """
    Test that a print() call with multiple lines produces a redirection prefix on each line.
    This is important because our third-party Rust reader reads until newline.
    """
    multiline_text = "Line one\nLine two\nLine three"
    with MultiplexedStream.setup_stream_redirection():
        print(multiline_text)
        sys.stdout.flush()
        sleep(0.2)

    captured = capfd.readouterr()
    # Remove any trailing newline and split the output into lines.
    lines = captured.out.strip().split("\n")
    # Each non-empty line should be prefixed with [PID:<pid>:stdout]
    expected_lines = ["Line one", "Line two", "Line three"]
    assert len(lines) == len(expected_lines)
    for line, expected in zip(lines, expected_lines, strict=False):
        # Check that the line starts with the prefix.
        assert line.startswith("[PID:")
        assert ":stdout]" in line
        # Extract the actual content after the prefix.
        content = line.split("]")[-1]
        assert content.strip() == expected
