import unittest

from app import BANNER, greet


class AppTest(unittest.TestCase):
    def test_greet(self):
        self.assertEqual(greet("world"), "hello, world!")

    def test_banner(self):
        self.assertTrue(BANNER.startswith("v"))


if __name__ == "__main__":
    unittest.main()
