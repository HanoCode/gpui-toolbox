// Duck Toolbox 插件：格式化（duck.text.formatter）。
//
// 契约：`handle(toolId, input)` 返回字符串结果；参数由宿主按
// `plugin.toml` 的 `[tools.io.params]` 收集后经 `duck.params` 注入。
// 纯函数：不读文件、不出网、不动剪贴板，所以本插件不申请任何能力。

/** 参数默认值：与清单里各参数的 default 一致，供参数缺失时兜底。 */
const DEFAULTS = { lang: "auto", indent: "2", minify: "false", sort_keys: "false" };

function takeParams() {
  const declared = (typeof duck.params === "object" && duck.params) || {};
  return { ...DEFAULTS, ...declared };
}

function sortKeysDeep(value) {
  if (Array.isArray(value)) return value.map(sortKeysDeep);
  if (value && typeof value === "object") {
    const out = {};
    for (const key of Object.keys(value).sort()) out[key] = sortKeysDeep(value[key]);
    return out;
  }
  return value;
}

function detectLang(text) {
  const t = text.trim();
  if (t.startsWith("{") || t.startsWith("[")) return "json";
  if (/^\s*<\?xml|^\s*<[a-zA-Z]/.test(t)) return "xml";
  if (/\b(select|insert|update|delete|create|alter|drop)\b/i.test(t)) return "sql";
  return "json";
}

const SQL_BREAK = new RegExp(
  "\\b(SELECT|FROM|WHERE|AND|OR|LEFT JOIN|RIGHT JOIN|INNER JOIN|JOIN|GROUP BY|ORDER BY|HAVING|LIMIT|INSERT INTO|VALUES|UPDATE|SET|DELETE FROM)\\b",
  "gi",
);

function formatSql(text, indent) {
  const pad = " ".repeat(indent);
  const lines = [];
  let depth = 0;
  for (const rawLine of text.replace(/\s+/g, " ").split(SQL_BREAK)) {
    const upper = rawLine.trim().toUpperCase();
    if (/^(FROM|WHERE|AND|OR|LEFT JOIN|RIGHT JOIN|INNER JOIN|JOIN|GROUP BY|ORDER BY|HAVING|LIMIT|VALUES|SET)$/.test(upper)) {
      depth = Math.max(0, depth === 0 && upper !== "FROM" ? 1 : depth);
      lines.push(pad.repeat(0) + rawLine.trim());
    } else {
      lines.push(rawLine.trim());
    }
  }
  // 关键字换行 + 子查询缩进（括号深度）。
  const out = [];
  for (const line of lines) {
    const opens = (line.match(/\(/g) || []).length;
    const closes = (line.match(/\)/g) || []).length;
    out.push(pad.repeat(Math.max(0, depth)) + line);
    depth += opens - closes;
  }
  return out.filter((l) => l.length > 0).join("\n").replace(/\n{2,}/g, "\n");
}

function formatXml(text, indent) {
  const pad = " ".repeat(indent);
  const tokens = text.replace(/>\s*</g, ">\n<").split("\n");
  let depth = 0;
  const out = [];
  for (const token of tokens) {
    const line = token.trim();
    if (!line) continue;
    if (/^<\//.test(line)) depth = Math.max(0, depth - 1);
    out.push(pad.repeat(depth) + line);
    if (/^<[^!?/]/.test(line) && !/\/>$/.test(line) && !/<\/[\w:-]+>$/.test(line)) depth += 1;
  }
  return out.join("\n");
}

function doFormat(params, input) {
  const lang = params.lang === "auto" ? detectLang(input) : params.lang;
  const indent = params.indent === "tab" ? "\t" : Number(params.indent) || 2;
  const minify = params.minify === "true";
  if (lang === "json") {
    let value;
    try {
      value = JSON.parse(input);
    } catch (error) {
      const pos = Number((String(error.message).match(/position (\d+)/) || [])[1] || NaN);
      const at = Number.isFinite(pos)
        ? duck.t("语法错误在第 {} 个字符附近：{}", pos, String(error.message))
        : duck.t("语法错误：{}", String(error.message));
      throw new Error(at);
    }
    if (params.sort_keys === "true") value = sortKeysDeep(value);
    return JSON.stringify(value, null, minify ? 0 : indent);
  }
  if (lang === "sql") return minify ? input.replace(/\s+/g, " ").trim() : formatSql(input, 4);
  if (lang === "xml") return minify ? input.replace(/>\s+</g, "><").trim() : formatXml(input, Number(indent) || 2);
  throw new Error(duck.t("不支持的语言：{}", lang));
}

export function handle(_toolId, input) {
  const text = String(input || "");
  if (!text.trim()) throw new Error(duck.t("请先填写要格式化的内容"));
  return doFormat(takeParams(), text);
}
