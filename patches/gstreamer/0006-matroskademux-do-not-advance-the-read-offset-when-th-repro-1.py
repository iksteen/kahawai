#!/usr/bin/env python3
"""Executable check for 0006.

The defect only becomes observable when the Tracks detour added by 0007
seeks back after a large Void. The combined fixture is therefore the real
reproducer for both patches; keep this convention-named entry point so patch
verification cannot silently classify 0006 as untested.
"""

import runpy
from pathlib import Path

runpy.run_path(
    Path(__file__).with_name(
        "0007-matroskademux-fetch-Tracks-that-follow-the-Clusters-repro-1.py"
    ),
    run_name="__main__",
)
