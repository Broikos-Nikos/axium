#!/usr/bin/env bash
set -euo pipefail

# ─────────────────────────────────────────────────────────────
# Axium — Full Setup Script
# Works on: Raspberry Pi (Debian/Pi OS) and Fedora/Arch PCs
#
# Usage:
#   sudo bash setup.sh            — full install / resume
#   sudo bash setup.sh --rebuild  — force recompile binary
# ─────────────────────────────────────────────────────────────

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; BOLD='\033[1m'; NC='\033[0m'
info()    { echo -e "${GREEN}[setup]${NC} $*"; }
warn()    { echo -e "${YELLOW}[warn]${NC}  $*"; }
error()   { echo -e "${RED}[error]${NC} $*" >&2; exit 1; }
ok()      { echo -e "  ${GREEN}✓${NC} $*"; }
bad()     { echo -e "  ${RED}✗${NC} $*"; }
note()    { echo -e "  ${YELLOW}⚠${NC} $*"; }
step()    { echo -e "\n${BOLD}── $* ${NC}"; }

FORCE_REBUILD="${1:-}"

# ── Must be root ──────────────────────────────────────────────
[ "$EUID" -eq 0 ] || error "Run with sudo:  sudo bash setup.sh"

# ── Resolve actual (non-root) user ───────────────────────────
ACTUAL_USER="${SUDO_USER:-$(logname 2>/dev/null || true)}"
[ -n "$ACTUAL_USER" ] || error "Cannot determine user. Run via sudo, not directly as root."
ACTUAL_HOME=$(getent passwd "$ACTUAL_USER" | cut -d: -f6)
[ -d "$ACTUAL_HOME" ] || error "Home directory '$ACTUAL_HOME' does not exist."

PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY="$PROJECT_DIR/target/release/axiom"
CONFIG="$PROJECT_DIR/config.json"
SERVICE_NAME="axium"
SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}.service"
CARGO_BIN="$ACTUAL_HOME/.cargo/bin/cargo"

# ── Detect distro and arch ────────────────────────────────────
[ -f /etc/os-release ] || error "Cannot detect distribution — /etc/os-release missing."
# shellcheck source=/dev/null
. /etc/os-release
DISTRO="${ID:-unknown}"
ARCH=$(uname -m)
TOTAL_RAM_MB=$(awk '/MemTotal/ {printf "%d", $2/1024}' /proc/meminfo)

echo ""
echo "  ╔══════════════════════════════════════════════╗"
echo "  ║         Axium — Full Setup Script            ║"
echo "  ╚══════════════════════════════════════════════╝"
echo ""
info "User:    $ACTUAL_USER  ($ACTUAL_HOME)"
info "Project: $PROJECT_DIR"
info "Distro:  $DISTRO  |  Arch: $ARCH  |  RAM: ${TOTAL_RAM_MB} MB"

# ═══════════════════════════════════════════════════════════════
step "1/9  Pre-flight checks"
# ═══════════════════════════════════════════════════════════════

# config.json — copy from example if missing, or generate a default
if [ ! -f "$CONFIG" ]; then
    if [ -f "$PROJECT_DIR/config.example.json" ]; then
        mv "$PROJECT_DIR/config.example.json" "$CONFIG"
        ok "Renamed config.example.json → config.json — edit it to add your API keys"
    else
        warn "config.example.json not found — generating a default config.json"
        cat > "$CONFIG" <<'DEFAULTCONFIG'
{
  "api_keys": { "anthropic": "sk-ant-...", "openai": "sk-proj-..." },
  "models": {
    "primary": "claude-sonnet-4-6", "primary_provider": "anthropic",
    "compactor": "gpt-4.1-mini", "compactor_provider": "openai",
    "classifier": "gpt-4.1-nano", "classifier_provider": "openai",
    "continuation": "", "continuation_provider": "",
    "review": "gpt-4.1-mini", "review_provider": "openai"
  },
  "available_models": {
    "anthropic": ["claude-sonnet-4-6","claude-opus-4-6","claude-haiku-4-5-20251001"],
    "openai": ["gpt-4.1","gpt-4.1-mini","gpt-4.1-nano","gpt-4o","gpt-4o-mini"]
  },
  "agent": { "name": "Axium", "soul": "" },
  "soul_file": "soul.md",
  "settings": {
    "token_limit": 80000, "max_tokens": 16384, "max_history_messages": 200,
    "terminal_timeout_secs": 120, "memory_file": "memory.md",
    "max_output_chars": 15000, "max_tool_iterations": 30,
    "max_input_chars": 12000, "max_retries": 2, "max_sessions": 50,
    "working_directory": "/home/yourname",
    "smtp_host": "", "smtp_port": 587, "smtp_user": "", "smtp_password": "",
    "smtp_from": "", "telegram_bot_token": "", "telegram_allowed_users": "",
    "telegram_enabled": false, "conversation_logging": false
  }
}
DEFAULTCONFIG
        ok "Created default config.json — edit it to add your API keys"
    fi
else
    ok "config.json found"
fi

# soul.md — copy from example if missing
if [ ! -f "$PROJECT_DIR/soul.md" ]; then
    if [ -f "$PROJECT_DIR/soul.example.md" ]; then
        cp "$PROJECT_DIR/soul.example.md" "$PROJECT_DIR/soul.md"
        ok "Created soul.md from soul.example.md"
    else
        echo "You are Axium, a precise and proactive Linux assistant." > "$PROJECT_DIR/soul.md"
        ok "Created default soul.md"
    fi
else
    ok "soul.md found"
fi

# memory.md — create empty if missing
if [ ! -f "$PROJECT_DIR/memory.md" ]; then
    echo "# Axium Memory" > "$PROJECT_DIR/memory.md"
    ok "Created empty memory.md"
else
    ok "memory.md found"
fi

# axium-skills/ — create directory if missing
if [ ! -d "$PROJECT_DIR/axium-skills" ]; then
    mkdir -p "$PROJECT_DIR/axium-skills"
    ok "Created axium-skills/ directory"
else
    ok "axium-skills/ found"
fi

# Valid JSON? Use jq if available; otherwise a basic brace-balance check.
if command -v jq &>/dev/null; then
    jq empty "$CONFIG" 2>/dev/null \
        && ok "config.json is valid JSON" \
        || error "config.json is not valid JSON — fix it and re-run."
else
    # Lightweight check: file must start with '{' and end with '}'
    FIRST=$(head -c 1 "$CONFIG" | tr -d '[:space:]')
    LAST=$(tail -c 1 "$CONFIG" | tr -d '[:space:]')
    if [ "$FIRST" = "{" ] && [ "$LAST" = "}" ]; then
        ok "config.json present (jq not installed — skipping deep validation)"
    else
        error "config.json looks malformed (does not start/end with braces) — fix it and re-run."
    fi
fi

# API keys present?
HAS_KEYS=false
grep -qE '"sk-ant-|sk-proj-' "$CONFIG" 2>/dev/null && HAS_KEYS=true
if [ "$HAS_KEYS" = true ]; then
    ok "API key(s) found in config.json"
else
    note "No API keys found in config.json"
    note "The service will exit immediately on start until keys are added."
    note "After setup finishes, open the browser UI → Settings to add them."
fi

# Internet connectivity?
if curl -s --max-time 8 https://static.rust-lang.org -o /dev/null 2>/dev/null; then
    ok "Internet reachable"
else
    note "Cannot reach rust-lang.org — Rust download may fail if not installed."
fi

# Port 3000 already in use by something other than us?
if ss -tlnp 2>/dev/null | grep -q ':3000 '; then
    note "Port 3000 already in use — will be freed when old service is replaced."
fi

# ═══════════════════════════════════════════════════════════════
step "2/9  System dependencies"
# ═══════════════════════════════════════════════════════════════

case "$DISTRO" in
    fedora|rhel|centos)
        dnf install -y gcc gcc-c++ make pkg-config curl git ca-certificates 2>&1 | tail -2
        ;;
    ubuntu|debian|raspbian|pop|linuxmint)
        apt-get update -qq 2>&1 | tail -1
        apt-get install -y --no-install-recommends \
            build-essential pkg-config curl git ca-certificates libssl-dev 2>&1 | tail -3
        ;;
    arch|manjaro|endeavouros)
        pacman -Syu --noconfirm base-devel pkgconf openssl curl git ca-certificates 2>&1 | tail -2
        ;;
    *)
        warn "Unknown distro '$DISTRO' — skipping package install."
        warn "Ensure gcc, pkg-config, curl, git are present."
        ;;
esac

# Verify essentials
MISSING_DEPS=()
for cmd in gcc curl git; do
    command -v "$cmd" &>/dev/null && ok "$cmd found" || MISSING_DEPS+=("$cmd")
done
[ ${#MISSING_DEPS[@]} -eq 0 ] || error "Missing after install: ${MISSING_DEPS[*]}. Install manually and re-run."

# ═══════════════════════════════════════════════════════════════
step "3/9  Swap"
# ═══════════════════════════════════════════════════════════════

if [ "$TOTAL_RAM_MB" -ge 2048 ]; then
    ok "RAM: ${TOTAL_RAM_MB} MB — swap not needed"
else
    SWAPFILE="/swapfile"
    SWAP_MB=4096
    NEED_SWAP=true

    if swapon --show 2>/dev/null | grep -q "$SWAPFILE"; then
        EXISTING_MB=$(( $(stat -c%s "$SWAPFILE" 2>/dev/null || echo 0) / 1024 / 1024 ))
        if [ "$EXISTING_MB" -ge "$SWAP_MB" ]; then
            ok "Swap already active: ${EXISTING_MB} MB"
            NEED_SWAP=false
        else
            info "Swap too small (${EXISTING_MB} MB) — recreating at ${SWAP_MB} MB..."
            swapoff "$SWAPFILE" 2>/dev/null || true
            rm -f "$SWAPFILE"
        fi
    fi

    if [ "$NEED_SWAP" = true ]; then
        info "Creating ${SWAP_MB} MB swapfile (needed for compilation)..."
        fallocate -l "${SWAP_MB}M" "$SWAPFILE" 2>/dev/null \
            || dd if=/dev/zero of="$SWAPFILE" bs=1M count="$SWAP_MB" status=progress
        chmod 600 "$SWAPFILE"
        mkswap "$SWAPFILE" > /dev/null
        swapon "$SWAPFILE"
        grep -q "$SWAPFILE" /etc/fstab || echo "$SWAPFILE none swap sw 0 0" >> /etc/fstab
    fi

    ACTIVE_SWAP=$(free -m | awk '/Swap:/ {print $2}')
    [ "$ACTIVE_SWAP" -gt 0 ] \
        && ok "Swap: ${ACTIVE_SWAP} MB active" \
        || error "Swap failed to activate — check filesystem supports swapfiles."
fi

# ═══════════════════════════════════════════════════════════════
step "4/9  Rust toolchain"
# ═══════════════════════════════════════════════════════════════

if [ -x "$CARGO_BIN" ]; then
    RUST_VER=$(su - "$ACTUAL_USER" -c "rustc --version" 2>/dev/null || echo "unknown")
    ok "Rust already installed: $RUST_VER"
else
    info "Installing Rust via rustup..."
    su - "$ACTUAL_USER" -c \
        'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'

    [ -x "$CARGO_BIN" ] \
        && ok "Rust installed: $(su - "$ACTUAL_USER" -c "rustc --version")" \
        || error "Rust installation failed.
  Check internet and try manually:
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi

# ═══════════════════════════════════════════════════════════════
step "5/9  Config patch"
# ═══════════════════════════════════════════════════════════════

# Fix any hardcoded /home/bro paths from the dev machine
if grep -q '/home/bro' "$CONFIG" 2>/dev/null; then
    sed -i "s|/home/bro|$ACTUAL_HOME|g" "$CONFIG"
    ok "Replaced /home/bro → $ACTUAL_HOME in config.json"
else
    ok "No hardcoded dev paths found"
fi

# Verify working_directory exists; reset to home if not
WD=$(grep -o '"working_directory" *: *"[^"]*"' "$CONFIG" | sed 's/.*: *"\([^"]*\)"/\1/' | head -1)
if [ -n "$WD" ] && [ ! -d "$WD" ]; then
    warn "working_directory '$WD' does not exist — resetting to $ACTUAL_HOME"
    sed -i "s|\"working_directory\": *\"[^\"]*\"|\"working_directory\": \"$ACTUAL_HOME\"|" "$CONFIG"
    WD="$ACTUAL_HOME"
fi
ok "working_directory: ${WD:-$ACTUAL_HOME}"

# ═══════════════════════════════════════════════════════════════
step "6/9  Build"
# ═══════════════════════════════════════════════════════════════

if [ -f "$BINARY" ] && [ "$FORCE_REBUILD" != "--rebuild" ]; then
    ok "Binary exists: $BINARY  ($(du -sh "$BINARY" | cut -f1))"
    info "Use 'sudo bash setup.sh --rebuild' to force recompilation."
else
    [ "$FORCE_REBUILD" = "--rebuild" ] && { info "Force rebuild — removing old binary."; rm -f "$BINARY"; }

    if [ "$TOTAL_RAM_MB" -ge 2048 ]; then
        JOBS=$(nproc)
        info "Building with $JOBS parallel jobs..."
    else
        JOBS=1
        info "Building with 1 job (~20-40 min on Pi Zero 2 W — please wait)..."
        info "Watch progress: journalctl -u $SERVICE_NAME -f  (another terminal)"
    fi

    BUILD_START=$(date +%s)
    BUILD_OK=true
    su - "$ACTUAL_USER" -c "
        source \$HOME/.cargo/env 2>/dev/null || true
        cd '$PROJECT_DIR'
        CARGO_INCREMENTAL=0 cargo build --release --jobs $JOBS
    " || BUILD_OK=false

    if [ "$BUILD_OK" = false ] || [ ! -f "$BINARY" ]; then
        error "Build failed. To see full output run:
    su - $ACTUAL_USER
    cd $PROJECT_DIR
    cargo build --release --jobs 1"
    fi

    BUILD_SECS=$(( $(date +%s) - BUILD_START ))
    ok "Build succeeded in ${BUILD_SECS}s  ($(du -sh "$BINARY" | cut -f1))"

    # On low-storage devices (SD card), clean build cache to reclaim space.
    # The binary is preserved; only intermediate artifacts are removed.
    DISK_FREE_MB=$(df -m "$PROJECT_DIR" | awk 'NR==2 {print $4}')
    if [ "$TOTAL_RAM_MB" -lt 2048 ] || [ "$DISK_FREE_MB" -lt 2048 ]; then
        info "Low storage detected — cleaning build cache to reclaim space..."
        CACHE_SIZE=$(du -sh "$PROJECT_DIR/target" 2>/dev/null | cut -f1)
        cp "$BINARY" "/tmp/axiom_preserve"
        su - "$ACTUAL_USER" -c "cd '$PROJECT_DIR' && source \$HOME/.cargo/env && cargo clean" 2>/dev/null || true
        mkdir -p "$PROJECT_DIR/target/release"
        mv "/tmp/axiom_preserve" "$BINARY"
        chmod +x "$BINARY"
        ok "Build cache cleaned (was $CACHE_SIZE, binary preserved)"
    fi
fi

# Verify binary is executable
[ -x "$BINARY" ] || { chmod +x "$BINARY"; ok "Set binary executable."; }

# Architecture sanity: catch accidental x86 binary on ARM host
if command -v file &>/dev/null; then
    BIN_INFO=$(file "$BINARY")
    case "$ARCH" in
        aarch64|armv7l)
            if echo "$BIN_INFO" | grep -qiE "ARM|aarch64"; then
                ok "Binary architecture: ARM ✓"
            else
                bad "Binary is x86-64 but this machine is $ARCH — deleting and recompiling..."
                rm -f "$BINARY"
                if [ "$TOTAL_RAM_MB" -ge 2048 ]; then
                    JOBS=$(nproc)
                else
                    JOBS=1
                    info "Building with 1 job (~20-40 min on Pi Zero 2 W — please wait)..."
                fi
                BUILD_START=$(date +%s)
                BUILD_OK=true
                su - "$ACTUAL_USER" -c "
                    source \$HOME/.cargo/env 2>/dev/null || true
                    cd '$PROJECT_DIR'
                    CARGO_INCREMENTAL=0 cargo build --release --jobs $JOBS
                " || BUILD_OK=false
                [ "$BUILD_OK" = true ] && [ -f "$BINARY" ] \
                    || error "Recompile failed. Run manually:  su - $ACTUAL_USER && cd $PROJECT_DIR && cargo build --release --jobs 1"
                BUILD_SECS=$(( $(date +%s) - BUILD_START ))
                ok "Recompiled for $ARCH in ${BUILD_SECS}s  ($(du -sh "$BINARY" | cut -f1))"
            fi
            ;;
        x86_64)
            echo "$BIN_INFO" | grep -qiE "x86-64|ELF 64.*x86" \
                && ok "Binary architecture: x86-64 ✓" \
                || note "Could not confirm binary arch — proceeding anyway."
            ;;
    esac
fi

# ═══════════════════════════════════════════════════════════════
step "7/9  SELinux / permissions"
# ═══════════════════════════════════════════════════════════════

# On Fedora/RHEL, systemd can't execute binaries from home directories
# without the correct SELinux type. Set it to bin_t.
if command -v getenforce &>/dev/null && [ "$(getenforce 2>/dev/null)" = "Enforcing" ]; then
    if command -v chcon &>/dev/null; then
        chcon -t bin_t "$BINARY"
        ok "SELinux: set bin_t context on binary"
    else
        note "SELinux is enforcing but chcon not found — service may fail with Permission denied."
    fi
else
    ok "SELinux not enforcing — no context change needed"
fi

# ═══════════════════════════════════════════════════════════════
step "8/9  Firewall"
# ═══════════════════════════════════════════════════════════════

FW_FOUND=false

if command -v ufw &>/dev/null; then
    FW_FOUND=true
    if ufw status 2>/dev/null | grep -qi "status: active"; then
        ufw allow 3000/tcp comment "Axium web UI" > /dev/null 2>&1
        ok "ufw: port 3000 allowed"
    else
        ok "ufw present but inactive — no rule needed"
    fi
fi

if command -v firewall-cmd &>/dev/null; then
    FW_FOUND=true
    if firewall-cmd --state 2>/dev/null | grep -q "running"; then
        firewall-cmd --add-port=3000/tcp --permanent > /dev/null 2>&1
        firewall-cmd --reload > /dev/null 2>&1
        ok "firewalld: port 3000 opened permanently"
    else
        ok "firewalld present but inactive — no rule needed"
    fi
fi

[ "$FW_FOUND" = false ] && ok "No firewall detected — port 3000 open by default"

# ═══════════════════════════════════════════════════════════════
step "9/10 Systemd service"
# ═══════════════════════════════════════════════════════════════

# Stop gracefully and free the port before restarting
systemctl stop "$SERVICE_NAME" 2>/dev/null || true
# Kill any stale process still holding port 3000
pkill -f "target/release/axiom" 2>/dev/null || true
sleep 1

cat > "$SERVICE_FILE" << EOF
[Unit]
Description=Axium Assistant
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$ACTUAL_USER
WorkingDirectory=$PROJECT_DIR
ExecStart=$BINARY
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=warn
StandardOutput=journal
StandardError=journal
SyslogIdentifier=axium

[Install]
WantedBy=multi-user.target
EOF

ok "Service file written"
ok "  ExecStart        : $BINARY"
ok "  User             : $ACTUAL_USER"
ok "  WorkingDirectory : $PROJECT_DIR"

systemctl daemon-reload
systemctl enable "$SERVICE_NAME" > /dev/null 2>&1
# Fix SELinux label so the binary is executable as a service
restorecon -v "$BINARY" > /dev/null 2>&1 || true
systemctl start "$SERVICE_NAME"

# Wait up to 15s for service to become active
info "Waiting for service to start..."
STARTED=false
for i in $(seq 1 8); do
    sleep 2
    if systemctl is-active --quiet "$SERVICE_NAME"; then
        ok "Service active after ${i}s"
        STARTED=true
        break
    fi
done

if [ "$STARTED" = false ]; then
    bad "Service did not start within 16s. Last log lines:"
    echo ""
    journalctl -u "$SERVICE_NAME" -n 25 --no-pager 2>/dev/null | sed 's/^/    /'
    echo ""
    error "Fix the issue above, then re-run.
  Common causes:
    - Binary is wrong architecture (delete it, run sudo bash setup.sh)
    - config.json has invalid JSON
    - API keys are missing (service exits immediately — add them to config.json first)"
fi

# ═══════════════════════════════════════════════════════════════
step "10/10 Health check"
# ═══════════════════════════════════════════════════════════════

sleep 2  # let the server bind

# Check bind address
BIND=$(ss -tlnp 2>/dev/null | grep ':3000' || echo "")
if [ -z "$BIND" ]; then
    bad "Port 3000 is not bound — service crashed after start."
    journalctl -u "$SERVICE_NAME" -n 15 --no-pager 2>/dev/null | sed 's/^/  /'
    error "Service is not listening. Common cause: missing/invalid API keys in config.json."
elif echo "$BIND" | grep -q '0.0.0.0:3000'; then
    ok "Listening on 0.0.0.0:3000 — LAN accessible ✓"
elif echo "$BIND" | grep -q '127.0.0.1:3000'; then
    bad "Listening on 127.0.0.1 only — LAN access will NOT work from your PC."
    note "This means an old binary is running (before the 0.0.0.0 bind change)."
    note "Force a rebuild:  sudo bash setup.sh --rebuild"
fi

# Test local HTTP
if command -v curl &>/dev/null; then
    HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" --max-time 5 http://127.0.0.1:3000/ 2>/dev/null || echo "000")
    if [ "$HTTP_CODE" = "200" ]; then
        ok "Web UI responding (HTTP 200) ✓"
    elif [ "$HTTP_CODE" = "000" ]; then
        note "Web UI not responding yet — may still be starting."
    else
        note "Web UI returned HTTP $HTTP_CODE — check logs if issues arise."
    fi
fi

# ═══════════════════════════════════════════════════════════════
# Done
# ═══════════════════════════════════════════════════════════════

LAN_IP=$(hostname -I 2>/dev/null | awk '{print $1}')
[ -n "$LAN_IP" ] || LAN_IP="<pi-ip>"

echo ""
echo -e "  ${BOLD}┌────────────────────────────────────────────────────┐${NC}"
echo -e "  ${BOLD}│  Axium is running!                                 │${NC}"
echo -e "  ${BOLD}│                                                    │${NC}"
echo -e "  ${BOLD}│  Browser (direct LAN):                             │${NC}"
printf "  ${BOLD}│    %-48s│${NC}\n" "http://${LAN_IP}:3000"
echo -e "  ${BOLD}│                                                    │${NC}"
echo -e "  ${BOLD}│  Browser (SSH tunnel — more secure):               │${NC}"
printf "  ${BOLD}│    %-48s│${NC}\n" "ssh -L 3000:localhost:3000 ${ACTUAL_USER}@${LAN_IP}"
echo -e "  ${BOLD}│    then open http://localhost:3000                 │${NC}"
echo -e "  ${BOLD}│                                                    │${NC}"
echo -e "  ${BOLD}│  Logs:    journalctl -u axium -f                   │${NC}"
echo -e "  ${BOLD}│  Restart: sudo systemctl restart axium             │${NC}"
echo -e "  ${BOLD}│  Rebuild: sudo bash setup.sh --rebuild             │${NC}"
echo -e "  ${BOLD}└────────────────────────────────────────────────────┘${NC}"
echo ""

if [ "$HAS_KEYS" = false ]; then
    echo -e "  ${YELLOW}⚠  No API keys found — open the URL above → Settings → add your key.${NC}"
    echo ""
fi
