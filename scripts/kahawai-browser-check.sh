#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
browser="${1:-}"

case "$browser" in
    chrome)
        project=chromium
        launcher=chromium
        options="{ channel: 'chrome' }"
        install='npm --prefix web exec playwright install -- chrome'
        ;;
    webkit)
        project=webkit
        launcher=webkit
        options='{}'
        install='npm --prefix web exec playwright install -- webkit'
        ;;
    *)
        printf 'usage: %s chrome|webkit\n' "$0" >&2
        exit 2
        ;;
esac

npm --prefix "$repo_dir/web" run build

if ! (
    cd "$repo_dir/web"
    node --input-type=module -e \
        "import { $launcher } from '@playwright/test'; const browser = await $launcher.launch($options); await browser.close()"
); then
    printf '%s\n' "$browser is not installed or cannot start under Playwright." >&2
    printf 'Install it once with: %s\n' "$install" >&2
    exit 1
fi

npm --prefix "$repo_dir/web" run test:product -- --project="$project"
