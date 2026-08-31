#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

npm --prefix "$repo_dir/web" run build

browser_path="$(
    cd "$repo_dir/web"
    node --input-type=module -e \
        "import { chromium } from '@playwright/test'; process.stdout.write(chromium.executablePath())"
)"
if ! test -x "$browser_path"; then
    printf '%s\n' 'Playwright Chromium is not installed.' >&2
    printf '%s\n' 'Install it once with: npm --prefix web exec playwright install -- --no-shell chromium' >&2
    exit 1
fi

npm --prefix "$repo_dir/web" run test:csp
