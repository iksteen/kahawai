"""What the hub reports with N mediahosts linked (NFR-2, see kahawai-fanout.sh)."""

import json
import sys

want, path = int(sys.argv[1]), sys.argv[2]
sats = json.load(open(path)).get("satellites", [])
fan = [s for s in sats if s["name"].startswith("fanout-")]
live = sum(1 for s in fan if s["connected"])
print("   fanout mediahosts enrolled : %d" % len(fan))
print("   of those connected         : %d/%d" % (live, want))
print("   pre-existing satellites    : %d (untouched)" % (len(sats) - len(fan)))
sys.exit(0 if live >= want else 1)
