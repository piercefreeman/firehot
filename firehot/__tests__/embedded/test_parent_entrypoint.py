import logging
import sys
import threading
from time import sleep

from firehot.embedded.parent_entrypoint import MultiplexedStream, check_thread_safety


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


def test_thread_safety_check(caplog):
    """
    Test that check_thread_safety correctly identifies and logs information about multiple threads.
    We create a background thread that sleeps, then verify the warning messages contain the
    expected thread information.
    """
    # Set up logging to capture warnings
    caplog.set_level(logging.WARNING)

    def background_task():
        # Simple task that just sleeps, giving us time to inspect it
        sleep(0.5)

    # Create and start a background thread
    background_thread = threading.Thread(
        target=background_task, name="TestBackgroundThread", daemon=True
    )
    background_thread.start()

    try:
        # Run the thread safety check
        check_thread_safety()

        # Verify the warning messages
        warnings = [record for record in caplog.records if record.levelname == "WARNING"]

        # Should have at least 2 warnings: initial warning and thread details
        assert len(warnings) >= 2, "Expected at least 2 warning messages"

        # Check the content of the first warning
        initial_warning = warnings[0].message
        assert "Detected 2 active threads before fork()" in initial_warning
        assert "deadlocks and memory corruption" in initial_warning

        # Check the thread details warnings
        thread_details = "".join(record.message for record in warnings[1:])

        # Verify both threads are logged
        assert "Name: MainThread" in thread_details
        assert "Name: TestBackgroundThread" in thread_details

        # Verify key thread information is present
        assert "ID:" in thread_details
        assert "Daemon:" in thread_details
        assert "Alive:" in thread_details
        assert "Stack Trace:" in thread_details

        # Verify our background task function appears in the stack trace
        assert "background_task" in thread_details

    finally:
        # Clean up by waiting for background thread to complete
        background_thread.join(timeout=1.0)
