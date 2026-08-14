#!/usr/bin/env bash
# Sets up a local build/run environment for cryptc.
set -euo pipefail
cd "$(dirname "$0")"

if command -v apt-get >/dev/null 2>&1; then
    echo "==> Installing system packages (fuse3, python3-venv) via apt"
    sudo apt-get update -qq
    sudo apt-get install -y fuse3 python3-venv python3-pip
elif command -v dnf >/dev/null 2>&1; then
    echo "==> Installing system packages (fuse3, python3) via dnf"
    sudo dnf install -y fuse3 python3
elif command -v brew >/dev/null 2>&1; then
    echo "==> Installing macFUSE via brew (requires a one-time security approval in System Settings)"
    brew install --cask macfuse
else
    echo "==> Unknown package manager: make sure fuse3/libfuse and python3-venv are installed manually" >&2
fi

echo "==> Creating Python virtualenv in .venv"
python3 -m venv .venv
. .venv/bin/activate
pip install --upgrade pip -q
pip install -r requirements.txt -q

echo
echo "Done. Activate with:  source .venv/bin/activate"
echo "Then:                 ./cryptc create myvault.cryptc"
echo "                      ./cryptc mount myvault.cryptc ~/vault"
