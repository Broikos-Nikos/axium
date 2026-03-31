#!/usr/bin/env bash
set -euo pipefail

echo "╔══════════════════════════════════════╗"
echo "║        Axium — Installer             ║"
echo "╚══════════════════════════════════════╝"
echo ""

# ── Check for git ──────────────────────────────────────────────────────────────
if ! command -v git &>/dev/null; then
    echo "► git not found. Installing..."
    if command -v apt-get &>/dev/null; then
        sudo apt-get update -qq && sudo apt-get install -y git
    elif command -v dnf &>/dev/null; then
        sudo dnf install -y git
    elif command -v pacman &>/dev/null; then
        sudo pacman -Sy --noconfirm git
    elif command -v zypper &>/dev/null; then
        sudo zypper install -y git
    else
        echo "✗ Could not install git — unsupported package manager. Install git manually and re-run."
        exit 1
    fi
    echo "✓ git installed."
else
    echo "✓ git found: $(git --version)"
fi

# ── Clone repo ─────────────────────────────────────────────────────────────────
REPO_URL="https://github.com/Broikos-Nikos/axium.git"
INSTALL_DIR="${1:-axium}"

if [ -d "$INSTALL_DIR/.git" ]; then
    echo "✓ Repo already cloned at '$INSTALL_DIR', pulling latest..."
    git -C "$INSTALL_DIR" pull
else
    echo "► Cloning Axium into '$INSTALL_DIR'..."
    git clone "$REPO_URL" "$INSTALL_DIR"
    echo "✓ Clone complete."
fi

# ── Run setup ──────────────────────────────────────────────────────────────────
cd "$INSTALL_DIR"
echo ""
echo "► Running setup.sh..."
echo ""
sudo bash setup.sh
