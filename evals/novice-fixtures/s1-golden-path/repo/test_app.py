import unittest

from app import add


class AppTest(unittest.TestCase):
    def test_add_positive(self):
        self.assertEqual(add(2, 3), 5)


if __name__ == "__main__":
    unittest.main()
