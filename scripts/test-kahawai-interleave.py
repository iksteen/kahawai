#!/usr/bin/env python3
"""Run with python3 scripts/test-kahawai-interleave.py."""
import importlib.util
from pathlib import Path
import unittest
import tempfile

spec = importlib.util.spec_from_file_location('interleave', Path(__file__).with_name('kahawai-interleave.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class LayoutTests(unittest.TestCase):
    def test_sustained_separation_reports_tracks_and_position(self):
        buckets = {s: {0: [s * 1000, s * 1000 + 500], 2: [32 * m.MIB + s * 100, 32 * m.MIB + s * 100 + 50]} for s in range(10, 20)}
        pair = m.summarize(buckets, 0, [2], 5)[0]
        self.assertEqual(pair['audio_stream'], 2)
        self.assertEqual(pair['hub']['longest_run_seconds'], 10)
        self.assertEqual(pair['hub']['examples'][0]['start_seconds'], 10)

    def test_high_bitrate_interleaving_is_not_a_gap(self):
        buckets = {s: {0: [0, 40 * m.MIB], 1: [100, 39 * m.MIB]} for s in range(10)}
        self.assertEqual(m.summarize(buckets, 0, [1], 5)[0]['hub']['affected_seconds'], 0)

    def test_isolated_and_missing_seconds_break_runs(self):
        buckets = {s: {0: [0, 100], 1: [32 * m.MIB, 33 * m.MIB]} for s in [0, 1, 3, 4, 6]}
        pair = m.summarize(buckets, 0, [1], 5)[0]
        self.assertEqual(pair['hub']['longest_run_seconds'], 2)
        self.assertEqual(pair['hub']['examples'], [])

    def test_smaller_transcoder_budget_and_alternate_audio(self):
        buckets = {s: {0: [0, 100], 1: [50, 150], 2: [4 * m.MIB, 5 * m.MIB]} for s in range(10)}
        pairs = m.summarize(buckets, 0, [1, 2], 5)
        self.assertEqual(pairs[0]['audio_stream'], 2)
        self.assertEqual(pairs[0]['hub']['affected_seconds'], 0)
        self.assertEqual(pairs[0]['transcoder']['longest_run_seconds'], 10)
        self.assertEqual(pairs[1]['transcoder']['affected_seconds'], 0)

    def test_no_comparable_timestamps_is_inconclusive(self):
        self.assertEqual(m.summarize({0: {0: [0, 10]}, 1: {1: [100, 110]}}, 0, [1], 5), [])

    def test_missing_packet_positions_are_not_a_clean_result(self):
        with tempfile.TemporaryDirectory() as temp:
            probe = Path(temp) / 'ffprobe'
            probe.write_text("#!/usr/bin/env python3\nimport sys\n"
                "if '-show_streams' in sys.argv: print('{\"streams\":[{\"index\":0,\"codec_type\":\"video\"},{\"index\":1,\"codec_type\":\"audio\"}]}')\n"
                "else: print('stream_index=0|dts_time=0|pos=N/A|size=100')\n")
            probe.chmod(0o755)
            result = m.scan(Path(temp) / 'media.mp4', str(probe), 5, 5)
            self.assertEqual(result['status'], 'inconclusive')
            self.assertEqual(result['coverage'], 'incomplete')
            self.assertEqual(result['packets_without_position_or_time'], 1)
            probe.write_text(probe.read_text().replace("else: print('stream_index=0|dts_time=0|pos=N/A|size=100')", "else: sys.exit(2)"))
            with self.assertRaisesRegex(RuntimeError, 'exited 2'):
                m.scan(Path(temp) / 'media.mp4', str(probe), 5, 5)


if __name__ == '__main__':
    unittest.main()
