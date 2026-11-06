#!/usr/bin/env bash
#
# 端到端验收脚本：逐条执行 todo.md §8 的 A1-A8，外加代码质量门（build / clippy / test）。
#
# 设计要点
#   * 每条用例独立 workspace 夹具 + 独立日志目录，用例之间不互相污染；
#   * 模型端点偶发 502/超时（实测约 1/3），单条用例失败会自动重试，最多 3 次尝试；
#   * 每条用例打印 PASS/FAIL，最后汇总计数；有 FAIL 时以非 0 退出码结束；
#   * 全程不修改 src/、Cargo.toml、.env、todo.md。
#
# 用法
#   ./scripts/acceptance.sh                  # 全量跑
#   ./scripts/acceptance.sh --only A1,A5     # 只跑指定用例（调试用）
#   ./scripts/acceptance.sh --attempts 3     # 每条用例最多尝试 3 次（默认 3）
#   ./scripts/acceptance.sh --no-quality     # 跳过 clippy / test
#   ./scripts/acceptance.sh --no-build       # 跳过开头 cargo build（已有二进制时）
#
# 产物
#   workspace/qa-artifacts/<case>/runN.out|err   每次尝试的 stdout / stderr
#   workspace/qa-artifacts/<case>/chain.txt      通过那一次的实际调用链
#   workspace/qa-artifacts/summary.txt           PASS/FAIL 汇总
#
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/tiny_ai_agent"
ART="$ROOT/workspace/qa-artifacts"
RUN_TIMEOUT="${RUN_TIMEOUT:-240}"   # 单次进程超时（秒）
ATTEMPTS=3                          # 单条用例最多尝试次数（含首次）
ONLY=""                             # 空 = 全部
DO_QUALITY=1
DO_BUILD=1

while [ $# -gt 0 ]; do
  case "$1" in
    --only)       ONLY="${2:-}"; shift 2 ;;
    --attempts)   ATTEMPTS="${2:-3}"; shift 2 ;;
    --no-quality) DO_QUALITY=0; shift ;;
    --no-build)   DO_BUILD=0; shift ;;
    -h|--help)    sed -n '2,30p' "$0"; exit 0 ;;
    *) echo "未知参数：$1" >&2; exit 2 ;;
  esac
done

# 中文输出不能乱码：日志与 grep 都用 UTF-8。
export LANG="${LANG:-C.UTF-8}"
export LC_ALL="${LC_ALL:-C.UTF-8}"

C_RED=$'\033[31m'; C_GRN=$'\033[32m'; C_YEL=$'\033[33m'; C_DIM=$'\033[2m'; C_OFF=$'\033[0m'
[ -t 1 ] || { C_RED=""; C_GRN=""; C_YEL=""; C_DIM=""; C_OFF=""; }

# 汇总数组（下标对应）
SUMMARY_IDS=()
SUMMARY_STATUS=()
SUMMARY_TRIES=()
SUMMARY_NOTE=()
FAIL_COUNT=0

pass() { printf '%sPASS%s' "$C_GRN" "$C_OFF"; }
fail() { printf '%sFAIL%s' "$C_RED" "$C_OFF"; }

note() { # 记录一条用例的结论行
  SUMMARY_IDS+=("$1"); SUMMARY_STATUS+=("$2"); SUMMARY_TRIES+=("$3"); SUMMARY_NOTE+=("${4:-}")
  [ "$2" = "FAIL" ] && FAIL_COUNT=$((FAIL_COUNT + 1))
  return 0
}

# ---------------------------------------------------------------- 基础设施

want_case() {
  [ -z "$ONLY" ] && return 0
  case ",$ONLY," in *",$1,"*) return 0 ;; *) return 1 ;; esac
}

# 取某个用例的 workspace 夹具，保证干净。$1=case 目录名 $2=相对项目根的路径
fresh_ws() {
  local dir="$ROOT/$1"
  rm -rf "$dir"
  mkdir -p "$dir"
  # 规范路径，避免 macOS /tmp 之类的符号链接干扰 starts_with 判断
  ( cd "$dir" && pwd -P )
}

# 启动一次 agent 进程。
#   $1=case id  $2=AGENT_WORKSPACE  $3=输入文件  $4=尝试序号
#   额外环境变量通过全局数组 EXTRA_ENV 传入
# 回显：进程退出码
exec_run() {
  local sid="$1" ws="$2" in="$3" run="$4"
  local d="$ART/$sid"
  local out="$d/run$run.out" err="$d/run$run.err"
  mkdir -p "$d"
  : >"$out"; : >"$err"
  ( cd "$ROOT" && env AGENT_WORKSPACE="$ws" ${EXTRA_ENV[@]+"${EXTRA_ENV[@]}"} \
      timeout "$RUN_TIMEOUT" "$BIN" ) <"$in" >"$out" 2>"$err"
  echo $?
}

# 把 stdout 里的"调用工具 → 返回"配对成可读调用链，写进 chain.txt
build_chain() {
  local out="$1" dest="$2"
  awk '
    /调用工具 / {
      line = $0
      sub(/^.*调用工具 /, "", line)
      sub(/，参数：/, " 参数=", line)
      cur = line
    }
    /工具 .* 返回/ {
      r = $0
      sub(/^.*返回（/, "", r); sub(/）：.*$/, "", r)
      if (cur != "") { print "  " cur "  →  返回[" r "]"; cur = "" }
    }
    END { if (cur != "") print "  " cur "  →  (未返回)" }
  ' "$out" >"$dest"
}

tool_call_count() { # $1=out 文件 $2=工具名
  grep -c "调用工具 $2，" "$1" 2>/dev/null || true
}

# 某个工具是否返回过错误
tool_errored() { # $1=out $2=工具名
  grep -q "工具 $2 返回（错误）" "$1"
}

has_answer() { grep -q '^助手> ' "$1"; }

answer_text() { sed -n 's/^助手> //p' "$1" | tr '\n' ' '; }

# 首次出现位置（行号），没有则输出 0
first_line_of() { grep -n "$2" "$1" 2>/dev/null | head -1 | cut -d: -f1; }

# 通用重试壳：$1=case id $2=用例函数名
retry_case() {
  local sid="$1" fn="$2" rc=1 run=1
  while [ "$run" -le "$ATTEMPTS" ]; do
    rm -f "$ART/$sid/reason.txt"
    "$fn" "$sid" "$run"
    rc=$?
    [ "$rc" -eq 0 ] && break
    if [ "$run" -lt "$ATTEMPTS" ]; then
      echo "  ${C_YEL}↻ $sid 第 $run 次尝试未通过，原因见下，重试…${C_OFF}"
      wait_retry "$sid"
    fi
    run=$((run + 1))
  done
  local tries=$((run > ATTEMPTS ? ATTEMPTS : run))
  if [ "$rc" -eq 0 ]; then
    echo "  $(pass) $sid （尝试 $tries/$ATTEMPTS 次）"
    note "$sid" PASS "$tries" ""
  else
    echo "  $(fail) $sid （尝试 $ATTEMPTS 次全部失败）"
    note "$sid" FAIL "$tries" "$(cat "$ART/$sid/reason.txt" 2>/dev/null | head -1)"
  fi
  return 0
}

# 失败后稍等，避免连续打满端点
wait_retry() { sleep 3; }

# 构造输入文件：$1=路径，其余参数=逐行输入
make_input() {
  local p="$1"; shift
  mkdir -p "$(dirname "$p")"
  : >"$p"
  local line
  for line in "$@"; do printf '%s\n' "$line" >>"$p"; done
  printf 'exit\n' >>"$p"
}

# 记录失败原因
say_fail() { printf '%s\n' "$2" >"$ART/$1/reason.txt"; echo "  ${C_RED}✗ $2${C_OFF}"; }

# ---------------------------------------------------------------- 用例 A1-A8

# A1：U1 计算 —— calculator 恰好一次，答案含 24192
case_a1() {
  local sid="$1" run="$2"
  local ws; ws="$(fresh_ws "workspace/qa-a1")"
  local in="$ART/$sid/input.txt"; make_input "$in" "帮我算一下 (1234 + 5678) × 3.5"
  EXTRA_ENV=()
  local rc; rc="$(exec_run "$sid" "$ws" "$in" "$run")"
  local out="$ART/$sid/run$run.out" err="$ART/$sid/run$run.err"
  build_chain "$out" "$ART/$sid/chain.txt"

  grep -q 'panicked at' "$err" && { say_fail "$sid" "进程 panic"; return 1; }
  [ "$rc" -ne 0 ] && { say_fail "$sid" "退出码 $rc（见 run$run.err）"; return 1; }

  local n; n="$(tool_call_count "$out" calculator)"
  [ "$n" -eq 1 ] || { say_fail "$sid" "calculator 调用 $n 次，期望 1 次"; return 1; }
  has_answer "$out" || { say_fail "$sid" "没有输出 助手> 最终回答"; return 1; }
  local ans; ans="$(answer_text "$out")"
  printf '%s' "$ans" | grep -q '24192' || { say_fail "$sid" "最终回答不含 24192：$ans"; return 1; }
  echo "  ${C_DIM}调用链：$(tr -s ' ' <"$ART/$sid/chain.txt" | tr '\n' ';')${C_OFF}"
  return 0
}

# A2：U2 天气 + 写文件 —— 依次 get_weather、write_file，weather.txt 存在且是天气文字
case_a2() {
  local sid="$1" run="$2"
  # 用默认 workspace 根，这样落点就是验收标准里写的 workspace/weather.txt
  local ws="$ROOT/workspace"
  mkdir -p "$ws"
  rm -f "$ws/weather.txt"
  ws="$( cd "$ws" && pwd -P )"
  local in="$ART/$sid/input.txt"; make_input "$in" "查一下香港今天的天气，把结果写到 weather.txt"
  EXTRA_ENV=()
  local rc; rc="$(exec_run "$sid" "$ws" "$in" "$run")"
  local out="$ART/$sid/run$run.out" err="$ART/$sid/run$run.err"
  build_chain "$out" "$ART/$sid/chain.txt"

  grep -q 'panicked at' "$err" && { say_fail "$sid" "进程 panic"; return 1; }
  [ "$rc" -ne 0 ] && { say_fail "$sid" "退出码 $rc（见 run$run.err）"; return 1; }

  local nw nf
  nw="$(tool_call_count "$out" get_weather)"; nf="$(tool_call_count "$out" write_file)"
  [ "$nw" -ge 1 ] || { say_fail "$sid" "没有调用 get_weather"; return 1; }
  [ "$nf" -ge 1 ] || { say_fail "$sid" "没有调用 write_file"; return 1; }
  # 顺序：get_weather 必须先于 write_file
  local lw lf
  lw="$(first_line_of "$out" '调用工具 get_weather')"; lf="$(first_line_of "$out" '调用工具 write_file')"
  [ -n "$lw" ] && [ -n "$lf" ] && [ "$lw" -lt "$lf" ] \
    || { say_fail "$sid" "调用顺序不对：get_weather@$lw write_file@$lf"; return 1; }

  [ -f "$ws/weather.txt" ] || { say_fail "$sid" "$ws/weather.txt 不存在"; return 1; }
  local body; body="$(cat "$ws/weather.txt")"
  [ -n "$body" ] || { say_fail "$sid" "weather.txt 为空"; return 1; }
  printf '%s' "$body" | grep -qE '温度|℃|°C|天气|晴|多云|阴|雨|度|°' \
    || { say_fail "$sid" "weather.txt 内容不像天气文字：$(printf '%s' "$body" | head -c 120)"; return 1; }
  echo "  ${C_DIM}weather.txt(${#body} 字符)：$(printf '%s' "$body" | head -c 100 | tr '\n' ' ')${C_OFF}"
  echo "  ${C_DIM}调用链：$(tr -s ' ' <"$ART/$sid/chain.txt" | tr '\n' ';')${C_OFF}"
  return 0
}

# A3：U3 闲聊 —— 无工具调用，一轮内回答
case_a3() {
  local sid="$1" run="$2"
  local ws; ws="$(fresh_ws "workspace/qa-a3")"
  local in="$ART/$sid/input.txt"; make_input "$in" "你好，你是谁？"
  EXTRA_ENV=()
  local rc; rc="$(exec_run "$sid" "$ws" "$in" "$run")"
  local out="$ART/$sid/run$run.out" err="$ART/$sid/run$run.err"
  build_chain "$out" "$ART/$sid/chain.txt"

  grep -q 'panicked at' "$err" && { say_fail "$sid" "进程 panic"; return 1; }
  [ "$rc" -ne 0 ] && { say_fail "$sid" "退出码 $rc（见 run$run.err）"; return 1; }

  local n; n="$(grep -c '调用工具 ' "$out" 2>/dev/null || true)"
  [ "$n" -eq 0 ] || { say_fail "$sid" "出现了 $n 次工具调用，期望 0 次"; return 1; }
  has_answer "$out" || { say_fail "$sid" "没有输出 助手> 最终回答"; return 1; }
  local ans; ans="$(answer_text "$out")"
  [ "${#ans}" -ge 2 ] || { say_fail "$sid" "回答过短：$ans"; return 1; }
  # 一轮内：只应出现"第 1/10 步"
  local steps; steps="$(grep -o '── 第 [0-9]*/' "$out" | wc -l | tr -d ' ')"
  [ "$steps" -eq 1 ] || { say_fail "$sid" "循环了 $steps 步，期望 1 步"; return 1; }
  echo "  ${C_DIM}回答：$(printf '%s' "$ans" | head -c 80)${C_OFF}"
  return 0
}

# A4：读不存在的文件 —— 工具报错、agent 说明、进程 0 退出不 panic
case_a4() {
  local sid="$1" run="$2"
  local ws; ws="$(fresh_ws "workspace/qa-a4")"
  local in="$ART/$sid/input.txt"
  make_input "$in" "帮我读一下 no_such_file_zzz.txt 的内容" 
  EXTRA_ENV=()
  local rc; rc="$(exec_run "$sid" "$ws" "$in" "$run")"
  local out="$ART/$sid/run$run.out" err="$ART/$sid/run$run.err"
  build_chain "$out" "$ART/$sid/chain.txt"

  grep -q 'panicked at' "$err" && { say_fail "$sid" "进程 panic（A4 明确不允许）"; return 1; }
  [ "$rc" -eq 0 ] || { say_fail "$sid" "退出码 $rc，期望 0（不 panic）"; return 1; }
  tool_errored "$out" read_file || { say_fail "$sid" "read_file 没有返回错误"; return 1; }
  has_answer "$out" || { say_fail "$sid" "没有输出 助手> 最终回答"; return 1; }
  local ans; ans="$(answer_text "$out")"
  printf '%s' "$ans" | grep -qE '不存在|找不到|没有找到|未找到|无法|失败|no_such' \
    || { say_fail "$sid" "回答没有说明文件不存在：$ans"; return 1; }
  echo "  ${C_DIM}回答：$(printf '%s' "$ans" | head -c 100)${C_OFF}"
  return 0
}

# A5：../ 越界 —— 工具拒绝并报路径越界
case_a5() {
  local sid="$1" run="$2"
  # 必须用默认 workspace 根，这样 ../Cargo.toml 才是"真的"越界
  local ws="$ROOT/workspace"
  mkdir -p "$ws"; ws="$( cd "$ws" && pwd -P )"
  local in="$ART/$sid/input.txt"
  make_input "$in" "请直接用 read_file 工具读取 ../Cargo.toml，不要自己判断路径是否合法，把工具返回的原始报错告诉我就行。"
  EXTRA_ENV=()
  local rc; rc="$(exec_run "$sid" "$ws" "$in" "$run")"
  local out="$ART/$sid/run$run.out" err="$ART/$sid/run$run.err"
  build_chain "$out" "$ART/$sid/chain.txt"

  grep -q 'panicked at' "$err" && { say_fail "$sid" "进程 panic"; return 1; }
  [ "$rc" -eq 0 ] || { say_fail "$sid" "退出码 $rc，期望 0"; return 1; }

  local ncalls; ncalls="$(tool_call_count "$out" read_file)"
  if [ "$ncalls" -ge 1 ]; then
    # 必须出现一条"参数里带 ../ 的调用"，并且它的返回是错误
    grep -q '调用工具 read_file，参数：.*\.\./' "$out" \
      || { say_fail "$sid" "没有一次调用真的传了 ../ 路径（模型可能自己改写了路径）"; return 1; }
    grep -q '工具 read_file 返回（错误）' "$out" \
      || { say_fail "$sid" "read_file 对 ../ 路径没有报错"; return 1; }
    # 错误文本必须明确提到越界（不能只是"文件不存在"）
    grep -E '工具 read_file 返回（错误）' "$out" \
      | grep -qE '越界|超出|不允许|不在工作目录|outside|escape|拒绝' \
      || { say_fail "$sid" "错误文本没提路径越界：$(grep -E '工具 read_file 返回（错误）' "$out" | tail -1)"; return 1; }
    # 测谎：越界读取如果真成功了，Cargo.toml 的内容会泄漏到日志里
    if grep -q '\[dependencies\]' "$out"; then
      say_fail "$sid" "越界读取疑似成功：日志里出现了 Cargo.toml 的 [dependencies] 内容"
      return 1
    fi
    echo "  ${C_DIM}工具拒绝：$(grep -E '路径越界' "$out" | tail -1 | head -c 130)${C_OFF}"
    return 0
  fi
  # 退一步：模型自己拒绝调用。A5 要求验证的是"工具拒绝越界路径"，
  # 没触发工具就等于没验到目标行为，判本次尝试无效并重试。
  local ans; ans="$(answer_text "$out")"
  say_fail "$sid" "模型自行拒绝了请求、没有调用 read_file，工具层越界未被验证（回答：$(printf '%s' "$ans" | head -c 60)）"
  return 1
}

# A6：AGENT_MAX_STEPS=2 跑 U2 —— 到上限停止并提示，不再发第 3 步请求
case_a6() {
  local sid="$1" run="$2"
  local ws; ws="$(fresh_ws "workspace/qa-a6")"
  local in="$ART/$sid/input.txt"
  make_input "$in" "查一下香港今天的天气，把结果写到 weather.txt"
  EXTRA_ENV=("AGENT_MAX_STEPS=2")
  local rc; rc="$(exec_run "$sid" "$ws" "$in" "$run")"
  local out="$ART/$sid/run$run.out" err="$ART/$sid/run$run.err"
  build_chain "$out" "$ART/$sid/chain.txt"

  grep -q 'panicked at' "$err" && { say_fail "$sid" "进程 panic"; return 1; }
  [ "$rc" -eq 0 ] || { say_fail "$sid" "退出码 $rc（见 run$run.err）"; return 1; }
  # 天花板：只应有 第 1/2、第 2/2 两步
  grep -q '── 第 1/2 步 ──' "$out" || { say_fail "$sid" "没有出现 第 1/2 步，AGENT_MAX_STEPS 可能未生效"; return 1; }
  grep -q '── 第 3' "$out" && { say_fail "$sid" "出现了第 3 步，超过上限仍在请求 API"; return 1; }
  local steps; steps="$(grep -o '── 第 [0-9]*/2 步 ──' "$out" | wc -l | tr -d ' ')"
  [ "$steps" -le 2 ] || { say_fail "$sid" "跑了 $steps 步，超过上限 2"; return 1; }
  if ! grep -qE '已达到最大循环次数' "$out"; then
    # 2 步内就干完了（模型把两个工具塞进同一步）：没触发上限，本条验不到目标行为，重试
    say_fail "$sid" "模型在 $steps 步内完成了 U2，未触发上限提示（无法验证 A6，重试）"
    return 1
  fi
  # 上限提示之后不应再有新的步骤标题
  local after
  after="$(sed -n '/已达到最大循环次数/,$p' "$out" | grep -c '── 第 ' || true)"
  [ "$after" -eq 0 ] || { say_fail "$sid" "达到上限后仍继续了 $after 步"; return 1; }
  echo "  ${C_DIM}步数 $steps/2，上限提示：$(grep -E '最大循环次数' "$out" | head -1 | head -c 90)${C_OFF}"
  return 0
}

# A7：连续两问，第二问引用第一问 —— 短期记忆生效
case_a7() {
  local sid="$1" run="$2"
  local ws; ws="$(fresh_ws "workspace/qa-a7")"
  local in="$ART/$sid/input.txt"
  make_input "$in" "我叫小王，请记住这个名字，只回复\"好的\"。" "我叫什么名字？"
  EXTRA_ENV=()
  local rc; rc="$(exec_run "$sid" "$ws" "$in" "$run")"
  local out="$ART/$sid/run$run.out" err="$ART/$sid/run$run.err"
  build_chain "$out" "$ART/$sid/chain.txt"

  grep -q 'panicked at' "$err" && { say_fail "$sid" "进程 panic"; return 1; }
  [ "$rc" -ne 0 ] && { say_fail "$sid" "退出码 $rc（见 run$run.err）"; return 1; }

  local answers; answers="$(grep -c '^助手> ' "$out")"
  [ "$answers" -ge 2 ] || { say_fail "$sid" "只有 $answers 个最终回答，期望 2 个"; return 1; }
  local ans; ans="$(answer_text "$out")"
  printf '%s' "$ans" | grep -q '小王' || { say_fail "$sid" "第二个回答没引用名字：$ans"; return 1; }
  echo "  ${C_DIM}回答序列：$(printf '%s' "$ans" | head -c 100)${C_OFF}"
  return 0
}

# A8：日志完整性 —— 每一步都有工具名、参数、结果
case_a8() {
  local sid="$1" run="$2"
  local ws; ws="$(fresh_ws "workspace/qa-a8")"
  local in="$ART/$sid/input.txt"; make_input "$in" "帮我算一下 (1234 + 5678) × 3.5"
  EXTRA_ENV=()
  local rc; rc="$(exec_run "$sid" "$ws" "$in" "$run")"
  local out="$ART/$sid/run$run.out" err="$ART/$sid/run$run.err"
  build_chain "$out" "$ART/$sid/chain.txt"

  grep -q 'panicked at' "$err" && { say_fail "$sid" "进程 panic"; return 1; }
  [ "$rc" -ne 0 ] && { say_fail "$sid" "退出码 $rc（见 run$run.err）"; return 1; }

  # 步骤标题 + 工具名 + 参数(JSON) + 结果
  grep -qE '── 第 [0-9]+/[0-9]+ 步 ──' "$out" || { say_fail "$sid" "缺少步骤标题日志"; return 1; }
  grep -qE '调用工具 [a-z_]+，参数：\{.*\}' "$out" || { say_fail "$sid" "缺少工具名/参数日志"; return 1; }
  grep -qE '工具 [a-z_]+ 返回（成功|错误）：.+' "$out" || { say_fail "$sid" "缺少工具结果日志"; return 1; }
  grep -qE '最终回答' "$out" || { say_fail "$sid" "缺少最终回答日志"; return 1; }
  echo "  ${C_DIM}日志样例：$(grep -E '调用工具|工具 .* 返回' "$out" | head -2 | tr '\n' ' | ' | head -c 160)${C_OFF}"
  return 0
}

# ---------------------------------------------------------------- 代码质量门

quality_gate() {
  echo
  echo "=== 代码质量 ==="

  # 1) build 无警告
  local blog="$ART/cargo-build.log"
  cargo build --manifest-path "$ROOT/Cargo.toml" >"$blog" 2>&1
  local brc=$?
  local warn; warn="$(grep -cE '^warning(:|\[)' "$blog" 2>/dev/null || true)"
  if [ "$brc" -eq 0 ] && [ "$warn" -eq 0 ]; then
    echo "  $(pass) Q1 cargo build 无警告"; note Q1 PASS 1 ""
  else
    echo "  $(fail) Q1 cargo build（exit=$brc，warning=$warn，见 ${blog#$ROOT/}）"
    note Q1 FAIL 1 "exit=$brc warning=$warn"
  fi

  [ "$DO_QUALITY" -eq 1 ] || { echo "  ${C_DIM}（--no-quality：跳过 clippy / test）${C_OFF}"; return 0; }

  # 2) clippy 零告警
  local clog="$ART/cargo-clippy.log"
  cargo clippy --manifest-path "$ROOT/Cargo.toml" --all-targets -- -D warnings >"$clog" 2>&1
  local crc=$?
  if [ "$crc" -eq 0 ]; then
    echo "  $(pass) Q2 cargo clippy --all-targets -- -D warnings"; note Q2 PASS 1 ""
  else
    echo "  $(fail) Q2 cargo clippy（exit=$crc，见 ${clog#$ROOT/}）"
    note Q2 FAIL 1 "exit=$crc"
  fi

  # 3) 单元测试
  local tlog="$ART/cargo-test.log"
  cargo test --manifest-path "$ROOT/Cargo.toml" >"$tlog" 2>&1
  local trc=$?
  local tsum; tsum="$(grep -E '^test result:' "$tlog" | tr '\n' ' ' | head -c 200)"
  if [ "$trc" -eq 0 ]; then
    echo "  $(pass) Q3 cargo test（$tsum）"; note Q3 PASS 1 ""
  else
    echo "  $(fail) Q3 cargo test（exit=$trc，见 ${tlog#$ROOT/}）"
    note Q3 FAIL 1 "exit=$trc"
  fi
  return 0
}

# ---------------------------------------------------------------- 主流程

main() {
  cd "$ROOT" || exit 2
  mkdir -p "$ART"

  if [ "$DO_BUILD" -eq 1 ]; then
    echo "=== 构建 ==="
    if ! cargo build >"$ART/cargo-build-prefix.log" 2>&1; then
      echo "$(fail) cargo build 失败，无法继续验收：见 ${ART#$ROOT/}/cargo-build-prefix.log"
      tail -20 "$ART/cargo-build-prefix.log"
      exit 1
    fi
    echo "  $(pass) cargo build"
  fi
  [ -x "$BIN" ] || { echo "$(fail) 找不到可执行文件 $BIN"; exit 1; }

  echo "可执行文件：$BIN"
  echo "超时：${RUN_TIMEOUT}s/次，最多 ${ATTEMPTS} 次尝试"
  echo

  echo "=== 验收用例 A1-A8 ==="
  want_case A1 && retry_case A1 case_a1
  want_case A2 && retry_case A2 case_a2
  want_case A3 && retry_case A3 case_a3
  want_case A4 && retry_case A4 case_a4
  want_case A5 && retry_case A5 case_a5
  want_case A6 && retry_case A6 case_a6
  want_case A7 && retry_case A7 case_a7
  want_case A8 && retry_case A8 case_a8

  [ "$DO_BUILD" -eq 1 ] && quality_gate

  # ---- 汇总
  echo
  echo "=== 汇总 ==="
  printf '%-4s %-6s %-4s %s\n' "用例" "结论" "尝试" "备注"
  local i
  : >"$ART/summary.txt"
  for i in "${!SUMMARY_IDS[@]}"; do
    printf '%-4s %-6s %-4s %s\n' "${SUMMARY_IDS[$i]}" "${SUMMARY_STATUS[$i]}" \
      "${SUMMARY_TRIES[$i]}" "${SUMMARY_NOTE[$i]}"
    printf '%s %s %s %s\n' "${SUMMARY_IDS[$i]}" "${SUMMARY_STATUS[$i]}" \
      "${SUMMARY_TRIES[$i]}" "${SUMMARY_NOTE[$i]}" >>"$ART/summary.txt"
  done

  local total="${#SUMMARY_IDS[@]}"
  local pcount=$((total - FAIL_COUNT))
  echo
  echo "总计 $total 项：${C_GRN}PASS $pcount${C_OFF} / ${C_RED}FAIL $FAIL_COUNT${C_OFF}"
  echo "日志目录：${ART#$ROOT/}/"

  if [ "$FAIL_COUNT" -gt 0 ]; then
    echo
    echo "失败项（最小复现）："
    for i in "${!SUMMARY_IDS[@]}"; do
      [ "${SUMMARY_STATUS[$i]}" = "FAIL" ] || continue
      local sid="${SUMMARY_IDS[$i]}"
      echo "  - $sid: AGENT_WORKSPACE=... ./target/debug/tiny_ai_agent < ${ART#$ROOT/}/$sid/input.txt   # ${SUMMARY_NOTE[$i]}"
    done
    exit 1
  fi
  echo "全部通过。"
  exit 0
}

main