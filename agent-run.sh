#!/usr/bin/env bash
set -euo pipefail

# ─────────────────────────────────────────────────────────────
# Axium — Quick Run Script (no sudo needed for most operations)
#
# Usage:
#   bash agent-run.sh              — build + restart service
#   bash agent-run.sh --rebuild    — clean + rebuild from scratch
#   bash agent-run.sh --stop       — stop the service
#   bash agent-run.sh --status     — show service status + health
#   bash agent-run.sh --logs       — tail live logs
# ─────────────────────────────────────────────────────────────

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; BOLD='\033[1m'; DIM='\033[2m'; NC='\033[0m'
ok()   { echo -e "  ${GREEN}✓${NC} $*"; }
warn() { echo -e "  ${YELLOW}⚠${NC} $*"; }
err()  { echo -e "  ${RED}✗${NC} $*" >&2; }
die()  { err "$@"; exit 1; }

PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY="$PROJECT_DIR/target/release/axiom"
SERVICE="axium"
PORT=3000
ACTION="${1:-run}"
HEADLESS=false

# Support --headless as a flag (can be second arg or first)
for arg in "$@"; do
  if [ "$arg" = "--headless" ]; then
    HEADLESS=true
  fi
done

cd "$PROJECT_DIR"
source "$HOME/.cargo/env" 2>/dev/null || true

# ── Subcommands ──────────────────────────────────────────────

case "$ACTION" in
  --stop)
    echo -e "${BOLD}Stopping Axium...${NC}"
    if systemctl is-active --quiet "$SERVICE" 2>/dev/null; then
      sudo systemctl stop "$SERVICE"
      ok "Service stopped"
    else
      ok "Service was not running"
    fi
    exit 0
    ;;

    --headless)
      # Accept --headless as a primary action for clarity
      ACTION="run"
      HEADLESS=true
      CLEAN=false
      ;;

  --status)
    echo -e "${BOLD}Axium Status${NC}"
    echo ""
    if systemctl is-active --quiet "$SERVICE" 2>/dev/null; then
      ok "Service: active"
      PID=$(systemctl show -p MainPID --value "$SERVICE" 2>/dev/null)
      if [ -n "$PID" ] && [ "$PID" != "0" ]; then
        RSS=$(ps -o rss= -p "$PID" 2>/dev/null | tr -d ' ')
        UPTIME=$(ps -o etime= -p "$PID" 2>/dev/null | tr -d ' ')
        [ -n "$RSS" ] && ok "Memory: $((RSS / 1024)) MB"
        [ -n "$UPTIME" ] && ok "Uptime: $UPTIME"
      fi
    else
      err "Service: inactive"
    fi
    if curl -s -o /dev/null -w '' --max-time 2 "http://127.0.0.1:$PORT/" 2>/dev/null; then
      HTTP=$(curl -s -o /dev/null -w "%{http_code}" --max-time 2 "http://127.0.0.1:$PORT/" 2>/dev/null)
      ok "HTTP: $HTTP on port $PORT"
    else
      err "Port $PORT not responding"
    fi
    if [ -f "$BINARY" ]; then
      ok "Binary: $(du -sh "$BINARY" | cut -f1)"
      ok "Built:  $(stat -c '%y' "$BINARY" 2>/dev/null | cut -d. -f1)"
    else
      err "Binary: not found"
    fi
    echo ""
    exit 0
    ;;

  --logs)
    exec journalctl -u "$SERVICE" -f --no-pager -n 50
    ;;

  --rebuild)
    echo -e "${BOLD}Clean rebuild requested${NC}"
    CLEAN=true
    ;;

  run|--run)
    CLEAN=false
    ;;

    --rebuild)
      echo -e "${BOLD}Clean rebuild requested${NC}"
      CLEAN=true
      ;;
  *)
    echo "Usage: bash agent-run.sh [--rebuild|--stop|--status|--logs]"
    exit 1
    ;;
esac

# ── Pre-flight ───────────────────────────────────────────────

echo ""

# ── Browser Auto-Open (unless headless) ─────────────
if [ "$HEADLESS" = false ]; then
  if command -v xdg-open &>/dev/null; then
    xdg-open "http://localhost:$PORT" &
    ok "Opened browser to http://localhost:$PORT"
  elif command -v sensible-browser &>/dev/null; then
    sensible-browser "http://localhost:$PORT" &
    ok "Opened browser to http://localhost:$PORT"
  else
    warn "Could not auto-open browser (no xdg-open or sensible-browser)"
  fi
fi
echo -e "${BOLD}  Axium — Quick Run${NC}"
echo ""

[ -f "$PROJECT_DIR/config.json" ] || die "config.json not found in $PROJECT_DIR"
command -v cargo &>/dev/null || die "cargo not found — install Rust first"

# ── Build ────────────────────────────────────────────────────

NEEDS_BUILD=false

if [ "$CLEAN" = true ]; then
  NEEDS_BUILD=true
elif [ ! -f "$BINARY" ]; then
  warn "Binary not found — building"
  NEEDS_BUILD=true
else
  # Check if any source file is newer than the binary
  NEWEST=$(find "$PROJECT_DIR/src" "$PROJECT_DIR/static" \
    "$PROJECT_DIR/Cargo.toml" "$PROJECT_DIR/Cargo.lock" \
    -newer "$BINARY" -type f 2>/dev/null | head -1)
  if [ -n "$NEWEST" ]; then
    NEEDS_BUILD=true
    echo -e "  ${DIM}Changed: ${NEWEST#"$PROJECT_DIR/"}${NC}"
  fi
fi

if [ "$NEEDS_BUILD" = true ]; then
  [ "$CLEAN" = true ] && { cargo clean 2>/dev/null || true; ok "Cleaned build cache"; }

  echo -e "  ${DIM}Building...${NC}"
  BUILD_START=$(date +%s)

  if ! cargo build --release 2>&1 | tail -5; then
    die "Build failed — fix errors above"
  fi

  [ -f "$BINARY" ] || die "Build produced no binary at $BINARY"

  BUILD_SECS=$(( $(date +%s) - BUILD_START ))
  ok "Built in ${BUILD_SECS}s ($(du -sh "$BINARY" | cut -f1))"
else
  ok "Binary up to date — skipping build"
fi

# ── SELinux ──────────────────────────────────────────────────

if command -v getenforce &>/dev/null && [ "$(getenforce 2>/dev/null)" = "Enforcing" ]; then
  if ! ls -Z "$BINARY" 2>/dev/null | grep -q 'bin_t'; then
    sudo chcon -t bin_t "$BINARY" 2>/dev/null && ok "SELinux: set bin_t" || warn "SELinux: chcon failed"
  fi
fi

# ── Restart ──────────────────────────────────────────────────

echo ""

if [ -f "/etc/systemd/system/${SERVICE}.service" ]; then
  # Stop cleanly
  if systemctl is-active --quiet "$SERVICE" 2>/dev/null; then
    sudo systemctl stop "$SERVICE" 2>/dev/null || true
    for _ in 1 2 3; do
      ss -tlnp 2>/dev/null | grep -q ":${PORT} " || break
      sleep 1
    done
  fi

  # Kill anything still holding the port
  if ss -tlnp 2>/dev/null | grep -q ":${PORT} "; then
    sudo fuser -k "${PORT}/tcp" 2>/dev/null || true
    sleep 1
  fi

  sudo systemctl start "$SERVICE"

  # Wait and verify
  echo -ne "  Starting."
  STARTED=false
  for _ in $(seq 1 10); do
    sleep 1
    echo -n "."
    if systemctl is-active --quiet "$SERVICE" 2>/dev/null; then
      STARTED=true
      break
    fi
  done
  echo ""

  if [ "$STARTED" = false ]; then
    err "Service did not start"
    echo ""
    journalctl -u "$SERVICE" -n 15 --no-pager 2>/dev/null | sed 's/^/    /'
    die "Check logs above"
  fi

  ok "Service started"

  sleep 1
  HTTP=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 "http://127.0.0.1:$PORT/" 2>/dev/null || echo "000")
  if [ "$HTTP" = "200" ]; then
    ok "Web UI ready (HTTP 200)"
  elif [ "$HTTP" = "000" ]; then
    warn "Port $PORT not responding yet — may still be starting"
  else
    warn "Web UI returned HTTP $HTTP"
  fi

else
  # No systemd service — run directly
  warn "No systemd service found — running directly"
  pkill -f "target/release/axiom" 2>/dev/null || true
  sleep 1

  RUST_LOG=info nohup "$BINARY" > /tmp/axium.log 2>&1 &
  AGENT_PID=$!
  sleep 2

  if kill -0 "$AGENT_PID" 2>/dev/null; then
    ok "Running (PID $AGENT_PID, log: /tmp/axium.log)"
  else
    die "Process exited immediately — check /tmp/axium.log"
  fi
fi

# ── Summary ──────────────────────────────────────────────────

LAN_IP=$(hostname -I 2>/dev/null | awk '{print $1}')
[ -n "$LAN_IP" ] || LAN_IP="<this-machine>"

echo ""
echo -e "  ${GREEN}Axium is running${NC}"
echo -e "  ${DIM}Local:   http://localhost:$PORT${NC}"
echo -e "  ${DIM}Network: http://${LAN_IP}:$PORT${NC}"
echo -e "  ${DIM}Logs:    bash agent-run.sh --logs${NC}"
echo ""
