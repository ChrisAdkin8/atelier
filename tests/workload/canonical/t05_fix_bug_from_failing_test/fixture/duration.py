"""Format an integer number of seconds as a human-readable duration."""


def format_duration(seconds):
    """Format `seconds` as 'XhYm', 'Xm', or 'Xh' depending on magnitude.

    Examples:
      format_duration(0)      -> "0m"
      format_duration(1500)   -> "25m"
      format_duration(7200)   -> "2h"
      format_duration(5400)   -> "1h30m"
    """
    hours = seconds // 3600
    minutes = (seconds % 3600) // 60
    if hours == 0:
        return f"{minutes}m"
    return f"{hours}h{minutes}m"
