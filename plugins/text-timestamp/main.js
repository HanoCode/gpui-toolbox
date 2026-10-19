// Duck Toolbox 插件：时间戳工具（duck.text.timestamp）。
//
// 契约：`handle(toolId, input)` 返回字符串结果；参数经 `duck.params` 注入。

const DEFAULTS = { mode: "to_date", unit: "s" };

function takeParams() {
  const declared = (typeof duck.params === "object" && duck.params) || {};
  return { ...DEFAULTS, ...declared };
}

function pad2(n) { return String(n).padStart(2, "0"); }

function formatDate(ms) {
  const d = new Date(ms);
  return duck.t("{}年{}月{}日 {}:{}:{}（本地时区）",
    d.getFullYear(), pad2(d.getMonth() + 1), pad2(d.getDate()),
    pad2(d.getHours()), pad2(d.getMinutes()), pad2(d.getSeconds()));
}

function doTimestamp(params, input) {
  if (params.mode === "now") {
    const ms = Date.now();
    return (params.unit === "ms" ? String(ms) : String(Math.floor(ms / 1000))) + "\n" + formatDate(ms);
  }
  const text = (input || "").trim();
  if (!text) throw new Error(duck.t("请填写时间戳或日期"));
  if (params.mode === "to_date") {
    let ms = Number(text);
    if (!Number.isFinite(ms)) throw new Error(duck.t("无法识别的时间戳：{}", text));
    if (params.unit !== "ms") ms *= 1000;
    return formatDate(ms);
  }
  const parsed = Date.parse(text.replace(" ", "T"));
  if (Number.isNaN(parsed)) throw new Error(duck.t("无法识别的日期：{}", text));
  return params.unit === "ms"
    ? String(parsed) + " ms"
    : String(Math.floor(parsed / 1000)) + " s";
}

export function handle(_toolId, input) {
  return doTimestamp(takeParams(), String(input || ""));
}
