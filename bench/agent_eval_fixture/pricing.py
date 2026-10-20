"""Small source fixture for live-agent answer and edit tasks."""


def sum_prices(items):
    return sum(item["price"] for item in items)
