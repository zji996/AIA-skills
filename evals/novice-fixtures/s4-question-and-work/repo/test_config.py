import unittest

from config import DEFAULT_TIMEOUT_SECONDS


class ConfigTest(unittest.TestCase):
    def test_timeout(self):
        self.assertEqual(DEFAULT_TIMEOUT_SECONDS, 30)


if __name__ == "__main__":
    unittest.main()
