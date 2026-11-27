import unittest

from mod_a import alpha
from mod_b import beta


class AllTest(unittest.TestCase):
    def test_alpha(self):
        self.assertEqual(alpha(), "done-a")

    def test_beta(self):
        self.assertEqual(beta(), "done-b")


if __name__ == "__main__":
    unittest.main()
