#!/usr/bin/env python3
"""Executable check for the upstream 0000 prerequisite.

0001's negative-running-time fixture takes the branch made safe by 0000, so
the same process-abort reproducer is the observable proof for both records.
"""

import runpy
from pathlib import Path

runpy.run_path(
    Path(__file__).with_name(
        "0001-hlssink3-don-t-unwrap-PTS-of-a-fragment-s-first-buff-repro-2.py"
    ),
    run_name="__main__",
)
