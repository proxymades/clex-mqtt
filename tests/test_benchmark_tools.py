#!/usr/bin/env python3
"""Validate portable CPU counters without contacting external services."""
import importlib.util
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("bench_diagnostics", ROOT / "scripts/bench_diagnostics.py")
bench = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bench)

class ProcessStatsTests(unittest.TestCase):
    def test_linux_uses_fractional_cpu_ticks_and_resident_pages(self):
        fields = ["0"] * 30
        fields[0], fields[11], fields[12], fields[21] = "S", "123", "77", "10"
        fixture = "123 (test (process) name) " + " ".join(fields)
        with patch.object(bench.sys, "platform", "linux"), patch.object(bench.Path, "read_text", return_value=fixture), patch.object(bench.os, "sysconf", side_effect=lambda key: {"SC_CLK_TCK": 100, "SC_PAGE_SIZE": 4096}[key]):
            self.assertEqual(bench.process_stats(123), (40.0, 2.0))

    def test_portable_ps_fallback_keeps_subsecond_values(self):
        result = SimpleNamespace(stdout="1024 00:00.12", returncode=0)
        with patch.object(bench.sys, "platform", "darwin"), patch.object(bench.subprocess, "run", return_value=result):
            self.assertEqual(bench.process_stats(123), (1024, .12))

    def test_cpu_clock_formats(self):
        for value, expected in (("0.12", .12), ("01:02.5", 62.5), ("01:02:03", 3723), ("1-01:02:03", 90123)):
            self.assertEqual(bench.cpu_seconds(value), expected)

if __name__ == "__main__":
    unittest.main(verbosity=2)
