// Duck Toolbox 插件：Cron 表达式（duck.text.cron-gen）。
//
// 契约：`handle(toolId, input)` 返回字符串结果；参数经 `duck.params` 注入。
//
// 与参考仓 `CronGen.vue` + `cron_next_runs` 对齐的两件事：
// ① 五个字段（分 时 日 月 周）都能填，`每月指定日` / `每年指定月日` 两个模板才做得出来；
// ② 结果里给**未来 10 次执行时间**——只给表达式等于把「对不对」留给用户自己心算。
//
// 匹配口径逐条照抄后端 `field_match`：`*`、`a-b`、`a,b`、`*/n` 四种；`a/n` 两边都不支持
// （口径一致比多支持一格重要）。唯一有意偏离：`7` 也当周日——周字段的标签写着
// 「7=周日」，不认它的话「每周日」会一条执行时间都算不出来。

const DEFAULTS = {
  preset: "daily",
  hour: "9",
  minute: "0",
  weekday: "1",
  day: "1",
  month: "1",
};

const WEEKDAYS = ["日", "一", "二", "三", "四", "五", "六"];

/** 未来执行时间看几条（参考页 `count: 10`）。 */
const NEXT_COUNT = 10;
/** 分钟级推进的上限：连 `0 0 29 2 *` 这种都够（四年内必有一次）。 */
const MAX_ATTEMPTS = 100000;

function takeParams() {
  const declared = (typeof duck.params === "object" && duck.params) || {};
  return { ...DEFAULTS, ...declared };
}

function clampInt(value, fallback, min, max) {
  const n = Math.floor(Number(value));
  if (!Number.isFinite(n)) return fallback;
  return Math.min(max, Math.max(min, n));
}

function describeCron(expr) {
  const parts = expr.trim().split(/\s+/);
  if (parts.length !== 5) return duck.t("字段数应为 5（分 时 日 月 周），当前 {} 个", parts.length);
  const [min, hour, dom, mon, dow] = parts;
  const lines = [];
  lines.push(min === "*" ? duck.t("每分钟") : duck.t("第 {} 分钟", min));
  lines.push(hour === "*" ? duck.t("每小时") : duck.t("{} 点", hour));
  // 日 + 月都限定时是「每年」而不是「每月…、某月」，分开念会读成两条规则。
  if (dom !== "*" && mon !== "*") {
    lines.push(duck.t("每年 {} 月 {} 号", mon, dom));
  } else {
    lines.push(dom === "*" ? duck.t("每日") : duck.t("每月 {} 号", dom));
    lines.push(mon === "*" ? duck.t("每月") : duck.t("{} 月", mon));
  }
  if (dow !== "*") {
    // 展开「1-5」这类连续区间（只处理单段简单范围，其余原样保留）。
    const days = [];
    for (const part of dow.split(",")) {
      const range = part.match(/^(\d+)-(\d+)$/);
      if (range) {
        const lo = Number(range[1]);
        const hi = Number(range[2]);
        for (let d = lo; d <= hi && d - lo < 7; d++) days.push(duck.t("周{}", duck.t(WEEKDAYS[d % 7])));
      } else {
        const n = Number(part);
        days.push(Number.isFinite(n) && n >= 0 && n <= 7 ? duck.t("周{}", duck.t(WEEKDAYS[n % 7])) : part);
      }
    }
    lines.push(days.join(duck.t("、")));
  }
  lines.push(duck.t("表达式：{}", expr));
  return lines.join("\n");
}

/** 单个字段是否命中（与参考仓 `field_match` 同口径）。 */
function fieldMatch(expr, value, min) {
  const text = String(expr == null ? "*" : expr).trim();
  if (text === "" || text === "*") return true;
  for (const raw of text.split(",")) {
    const part = raw.trim();
    if (!part) continue;
    const range = part.match(/^(\d+)-(\d+)$/);
    if (range) {
      if (value >= Number(range[1]) && value <= Number(range[2])) return true;
      continue;
    }
    const step = part.match(/^\*\/(\d+)$/);
    if (step) {
      const n = Number(step[1]);
      if (n > 0 && (value - min) % n === 0) return true;
      continue;
    }
    if (Number(part) === value) return true;
  }
  return false;
}

function pad(n) {
  return String(n).padStart(2, "0");
}

function formatRun(date) {
  const stamp =
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ` +
    `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
  // 星期名走 `周{}` 那条译文，英文界面才不会冒出「星期一」。
  return duck.t("{}（{}）", stamp, duck.t("周{}", duck.t(WEEKDAYS[date.getDay()])));
}

/** 未来 N 次执行时间；表达式非法时给空数组（不抛错——描述那行已经提示过了）。 */
function nextRuns(expr, count) {
  const parts = expr.trim().split(/\s+/);
  if (parts.length !== 5) return [];
  const [minExpr, hourExpr, domExpr, monExpr, dowExpr] = parts;
  // 日 + 周同时限定时按「都要满足」（参考仓也是 `&&`，不按 crontab 的 OR 口径）。
  const dayHit = (date) =>
    fieldMatch(domExpr, date.getDate(), 1) &&
    (fieldMatch(dowExpr, date.getDay(), 0) || (date.getDay() === 0 && fieldMatch(dowExpr, 7, 0)));
  const runs = [];
  const cursor = new Date();
  // 参考仓从「下一分钟」起算，且秒/毫秒清零。
  cursor.setSeconds(0, 0);
  cursor.setMinutes(cursor.getMinutes() + 1);
  let attempts = 0;
  while (runs.length < count && attempts < MAX_ATTEMPTS) {
    if (fieldMatch(minExpr, cursor.getMinutes(), 0) && fieldMatch(hourExpr, cursor.getHours(), 0) && dayHit(cursor)) {
      runs.push(formatRun(cursor));
    }
    // **逐分钟推进太慢**：`每年 3 月 15 日` 要走 30 万分钟、2 月 29 日要走四年。
    // 所以整片跳过：月不对 → 跳到下月 1 号 0 点；日不对 → 跳到明天 0 点；
    // 时不对 → 跳到下一个整点。跳过的这些格本来就不该命中，口径不变。
    if (!fieldMatch(monExpr, cursor.getMonth() + 1, 1)) {
      cursor.setHours(0, 0, 0, 0);
      cursor.setDate(1);
      cursor.setMonth(cursor.getMonth() + 1);
    } else if (!dayHit(cursor)) {
      cursor.setHours(0, 0, 0, 0);
      cursor.setDate(cursor.getDate() + 1);
    } else if (!fieldMatch(hourExpr, cursor.getHours(), 0)) {
      cursor.setMinutes(0, 0, 0);
      cursor.setHours(cursor.getHours() + 1);
    } else {
      cursor.setMinutes(cursor.getMinutes() + 1);
    }
    attempts += 1;
  }
  return runs;
}

function withNextRuns(expr, description) {
  const runs = nextRuns(expr, NEXT_COUNT);
  const lines = [description, ""];
  lines.push(duck.t("未来执行时间（{} 条）", runs.length));
  if (runs.length === 0) {
    lines.push(duck.t("无匹配时间"));
    return lines.join("\n");
  }
  for (const run of runs) lines.push(run);
  return lines.join("\n");
}

function doCronGen(params, input) {
  const hour = clampInt(params.hour, 9, 0, 23);
  const minute = clampInt(params.minute, 0, 0, 59);
  const weekday = clampInt(params.weekday, 1, 1, 7);
  const day = clampInt(params.day, 1, 1, 31);
  const month = clampInt(params.month, 1, 1, 12);
  switch (params.preset) {
    case "every_minute":
      return withNextRuns("* * * * *", describeCron("* * * * *"));
    case "hourly":
      return withNextRuns(`${minute} * * * *`, describeCron(`${minute} * * * *`));
    case "daily":
      return withNextRuns(`${minute} ${hour} * * *`, describeCron(`${minute} ${hour} * * *`));
    case "weekly":
      return withNextRuns(
        `${minute} ${hour} * * ${weekday}`,
        describeCron(`${minute} ${hour} * * ${weekday}`),
      );
    case "workday":
      return withNextRuns(`${minute} ${hour} * * 1-5`, describeCron(`${minute} ${hour} * * 1-5`));
    case "monthly":
      return withNextRuns(
        `${minute} ${hour} ${day} * *`,
        describeCron(`${minute} ${hour} ${day} * *`),
      );
    case "yearly":
      return withNextRuns(
        `${minute} ${hour} ${day} ${month} *`,
        describeCron(`${minute} ${hour} ${day} ${month} *`),
      );
    case "custom": {
      const expr = (input || "").trim();
      if (!expr) throw new Error(duck.t("请在输入框填写要校验的表达式"));
      if (!/^[\d*,\-/\s]+$/.test(expr)) throw new Error(duck.t("表达式含非法字符（只允许数字 * , - / 和空格）"));
      return withNextRuns(expr, describeCron(expr));
    }
    default:
      throw new Error(duck.t("未知模板：{}", params.preset));
  }
}

export function handle(_toolId, input) {
  return doCronGen(takeParams(), String(input || ""));
}
