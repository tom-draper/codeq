import unittest

from pricing import sum_prices


class SumPricesTests(unittest.TestCase):
    def test_includes_active_items(self):
        self.assertEqual(sum_prices([{"price": 3}, {"price": 8}]), 11)

    def test_ignores_cancelled_items(self):
        self.assertEqual(sum_prices([
            {"price": 12, "cancelled": True}, {"price": 5}
        ]), 5)


if __name__ == "__main__":
    unittest.main()
