#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

npm --prefix "$repo_dir/web" run build

if ! (
    cd "$repo_dir/web"
    node --input-type=module -e \
        "import { chromium } from '@playwright/test'; const browser = await chromium.launch({ channel: 'chrome' }); await browser.close()"
); then
    printf '%s\n' 'Google Chrome is not installed for Playwright.' >&2
    printf '%s\n' 'Install it once with: npm --prefix web exec playwright install -- chrome' >&2
    exit 1
fi

npm --prefix "$repo_dir/web" run test:csp
