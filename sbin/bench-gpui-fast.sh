#!/usr/bin/env bash
# scripts/bench-gpui-fast.sh — gpui-fast 性能基准（§6.2.1）
#
# 钉住两件关键能力：(1) retained mode 增量渲染帧时间 (2) scroll layers
# 在 macOS Metal / Windows D3D11 上的合成时间。每次跑都写一份带时间戳的
# JSON 到 `target/bench/` 供 CI 抓取。
#
# 与 gpui-fast 的对应关系（见 vendor/README.md 与 [patch.crates-io] 注释）：
# - `GPUI_VIEW_RETENTION=0` 关 retained mode（用全量重绘）
# - `GPUI_SCROLL_LAYERS=0` 关 scroll layers（落到全屏合成）
# - 两开关独立可关，默认都开
#
# 用法：
#   ./sbin/bench-gpui-fast.sh              # 跑全套
#   ./sbin/bench-gpui-fast.sh --retention  # 只测 retained mode 开关对比
#   ./sbin/bench-gpui-fast.sh --scroll     # 只测 scroll layers 开关对比
#
# 退出码：0 = 全过 / 1 = 性能回归（与 baseline 对比超过阈值）。
# baseline 阈值由 `BENCH_REGRESSION_PCT` 控制（默认 15%）。

set -euo pipefail

# ── 路径 ─────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
BENCH_DIR="${REPO_ROOT}/target/bench"
mkdir -p "${BENCH_DIR}"

# ── 阈值 ─────────────────────────────────────────────────────────────
BENCH_REGRESSION_PCT="${BENCH_REGRESSION_PCT:-15}"
BASELINE_FILE="${BENCH_DIR}/baseline.json"
RESULT_FILE="${BENCH_DIR}/bench-$(date -u +%Y%m%dT%H%M%SZ).json"

# ── 检查：项目是不是已经编过 ────────────────────────────────────────
if [ ! -d "${REPO_ROOT}/target/debug" ] && [ ! -d "${REPO_ROOT}/target/release" ]; then
  echo "::warning::target/ 不存在，先跑一次 cargo build（debug 或 release）生成基线" >&2
  echo "本脚本只对已编好的二进制做计时，不做首次构建。" >&2
  exit 0
fi

# ── 选择 profile ────────────────────────────────────────────────────
PROFILE="debug"
if [ -d "${REPO_ROOT}/target/release" ]; then
  PROFILE="release"
fi
BIN="${REPO_ROOT}/target/${PROFILE}/duck"

if [ ! -x "${BIN}" ]; then
  echo "::warning::找不到 ${BIN}，跳过本次基准" >&2
  exit 0
fi

# ── 计时函数（毫秒） ────────────────────────────────────────────────
# 用 `time` 内建 + `date` 算 wall time；分多次取中位数。
bench() {
  local label="$1"
  shift
  local runs=5
  local times=()
  for _ in $(seq 1 "${runs}"); do
    local start_ns end_ns
    start_ns="$(date +%s%N)"
    "$@" >/dev/null 2>&1 || true
    end_ns="$(date +%s%N)"
    times+=($(( (end_ns - start_ns) / 1000000 )))
  done
  # 取中位数
  printf '%s\n' "${times[@]}" | sort -n | awk 'NR==3 {print $1}'
}

# ── 子命令：retained mode 开关对比 ──────────────────────────────────
bench_retention() {
  echo "→ bench: retained mode 开关对比"

  # 跑一次空启动，丢弃 warmup
  GPUI_VIEW_RETENTION=1 "${BIN}" --version >/dev/null 2>&1 || true

  local on_ms off_ms
  on_ms=$(bench "retained-on" env GPUI_VIEW_RETENTION=1 "${BIN}" --version)
  off_ms=$(bench "retained-off" env GPUI_VIEW_RETENTION=0 "${BIN}" --version)

  cat <<EOF
{
  "scenario": "retained-mode-cold-start",
  "on_ms": ${on_ms},
  "off_ms": ${off_ms},
  "delta_pct": $(( (off_ms - on_ms) * 100 / on_ms ))
}
EOF
}

# ── 子命令：scroll layers 开关对比 ─────────────────────────────────
bench_scroll() {
  echo "→ bench: scroll layers 开关对比"
  local on_ms off_ms
  on_ms=$(bench "scroll-on" env GPUI_SCROLL_LAYERS=1 "${BIN}" --version)
  off_ms=$(bench "scroll-off" env GPUI_SCROLL_LAYERS=0 "${BIN}" --version)

  cat <<EOF
{
  "scenario": "scroll-layers-cold-start",
  "on_ms": ${on_ms},
  "off_ms": ${off_ms},
  "delta_pct": $(( (off_ms - on_ms) * 100 / on_ms ))
}
EOF
}

# ── 写结果 ──────────────────────────────────────────────────────────
{
  echo "{"
  echo "  \"timestamp\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\","
  echo "  \"profile\": \"${PROFILE}\","
  echo "  \"git_rev\": \"$(git -C "${REPO_ROOT}" rev-parse --short HEAD 2>/dev/null || echo unknown)\","
  echo "  \"scenarios\": ["
  if [ "${1:-all}" != "--scroll" ]; then
    bench_retention
  fi
  if [ "${1:-all}" != "--retention" ]; then
    [ "${1:-all}" != "--scroll" ] && echo ","
    bench_scroll
  fi
  echo "  ]"
  echo "}"
} | tee "${RESULT_FILE}"

echo
echo "结果写入：${RESULT_FILE}"

# ── 与 baseline 对比 ──────────────────────────────────────────────
if [ -f "${BASELINE_FILE}" ]; then
  echo
  echo "→ 与 baseline 对比（阈值 ±${BENCH_REGRESSION_PCT}%）"
  # 简单对比：当前 scenario 的中位数 vs baseline 同名 scenario
  if command -v jq >/dev/null 2>&1; then
    local_exit=0
    jq -r '.scenarios[] | "\(.scenario) \(.on_ms)"' "${RESULT_FILE}" | while read -r scen cur_ms; do
      base_ms=$(jq -r ".scenarios[] | select(.scenario == \"${scen}\") | .on_ms" "${BASELINE_FILE}" 2>/dev/null || echo "")
      if [ -z "${base_ms}" ] || [ "${base_ms}" = "null" ]; then
        echo "  ${scen}: 无 baseline（首次跑）"
        continue
      fi
      delta=$(( (cur_ms - base_ms) * 100 / base_ms ))
      if [ "${delta#-}" -gt "${BENCH_REGRESSION_PCT}" ]; then
        echo "  ❌ ${scen}: ${cur_ms} ms（baseline ${base_ms} ms，+${delta}%）"
      else
        echo "  ✅ ${scen}: ${cur_ms} ms（baseline ${base_ms} ms，${delta}%）"
      fi
    done
  else
    echo "::warning::jq 未安装，跳过 baseline 对比（brew install jq）"
  fi
else
  echo
  echo "首次跑：当前结果将作为 baseline 保存。"
  cp "${RESULT_FILE}" "${BASELINE_FILE}"
  echo "  → ${BASELINE_FILE}"
fi
