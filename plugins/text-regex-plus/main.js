// Duck Toolbox 插件：正则测试（duck.text.regex-plus）。
//
// 契约：`handle(toolId, input)` 返回字符串结果；参数经 `duck.params` 注入。

const DEFAULTS = { flags: "g", replacement: "" };

function takeParams() {
  const declared = (typeof duck.params === "object" && duck.params) || {};
  return { ...DEFAULTS, ...declared };
}

function doRegex(params, input) {
  const pattern = (params.pattern || "").trim();
  if (!pattern) throw new Error(duck.t("请填写正则表达式"));
  const flags = (params.flags || "g").replace(/[^gimsuy]/g, "") || "g";
  let regex;
  try {
    regex = new RegExp(pattern, flags);
  } catch (error) {
    throw new Error(duck.t("正则无效：{}", String(error.message)));
  }
  const lines = [];
  if (params.replacement) {
    lines.push(duck.t("替换预览："));
    lines.push(input.replace(regex, params.replacement));
    lines.push("");
  }
  let count = 0;
  if (flags.includes("g")) {
    for (const match of input.matchAll(regex)) {
      count++;
      if (count > 500) { lines.push(duck.t("…（超过 500 条已截断）")); break; }
      const groups = match.slice(1).map((g, i) => duck.t("组{}={}", i + 1, g === undefined ? "—" : g));
      lines.push(duck.t("[{}] {} {}", match.index, match[0], groups.join(" ")));
    }
  } else {
    const match = regex.exec(input);
    if (match) {
      count = 1;
      const groups = match.slice(1).map((g, i) => duck.t("组{}={}", i + 1, g === undefined ? "—" : g));
      lines.push(duck.t("[{}] {} {}", match.index, match[0], groups.join(" ")));
    }
  }
  if (count === 0 && !params.replacement) return duck.t("无匹配");
  lines.unshift(duck.t("匹配 {} 处", count), "");
  return lines.join("\n");
}

export function handle(_toolId, input) {
  const text = String(input || "");
  const params = takeParams();
  if (!text) throw new Error(duck.t("请先填写测试文本"));
  return doRegex(params, text);
}
