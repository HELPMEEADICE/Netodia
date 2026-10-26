"""Memory-report accounting tests, independent of Rust and /proc availability."""

import importlib.util
from pathlib import Path
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location(
    "profile_ui_memory", Path(__file__).with_name("profile-ui-memory.py")
)
PROFILE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROFILE)


class MemoryAccountingTests(unittest.TestCase):
    def test_counters_are_bytes_and_non_counters_are_ignored(self):
        result = PROFILE.parse_kib(
            "Name: Netodia\nRss: 100 kB\nAnonymous: 30 kB\nSwap: 2 kB\nThreads: 8\n"
        )
        self.assertEqual(result, {"Rss": 102400, "Anonymous": 30720, "Swap": 2048})

    def test_no_overlap_is_added_by_parser(self):
        result = PROFILE.parse_kib("Rss: 100 kB\nPss: 80 kB\nPss_Anon: 30 kB\n")
        self.assertEqual(result["Pss"], 81920)

    def test_cover_is_a_valid_rgb_png_with_expected_dimensions(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "cover.png"
            PROFILE.write_cover(path, 17)
            image = path.read_bytes()
        self.assertEqual(image[:8], b"\x89PNG\r\n\x1a\n")
        self.assertEqual(PROFILE.struct.unpack("!2I", image[16:24]), (256, 256))
        self.assertEqual(image[24:26], bytes((8, 2)))


if __name__ == "__main__":
    unittest.main()
