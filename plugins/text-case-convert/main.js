// Duck Toolbox 插件：大小写转换（duck.text.case-convert）。
//
// 契约：`handle(toolId, input)` 返回字符串结果；参数经 `duck.params` 注入。
//
// 结果是一张 **Markdown 表**（参考项目 `CaseConvert.vue` 把六种风格并列在一个
// 结果卡里）：宿主 `[io.output] kind = "table"` 会把它解析成真表格，六种命名
// 风格一眼可比，而不是选一种看一种。

/** 拆词：camelCase / snake_case / kebab-case / 空白 / 中划线混写都认。
 *  连续大写按「大写后跟小写」断开：HTTPServer → HTTP Server。 */
function splitWords(text) {
  return String(text)
    .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
    .replace(/([A-Z])([A-Z][a-z])/g, "$1 $2")
    .split(/[^A-Za-z0-9]+/)
    .filter(Boolean);
}

/** 六种风格（与参考页面一一对应）。 */
const STYLES = [
  ["camelCase", (w) => w.map((x, i) => (i === 0 ? x : x[0].toUpperCase() + x.slice(1))).join("")],
  ["PascalCase", (w) => w.map((x) => x[0].toUpperCase() + x.slice(1)).join("")],
  ["snake_case", (w) => w.join("_")],
  ["SCREAMING_SNAKE", (w) => w.join("_").toUpperCase()],
  ["kebab-case", (w) => w.join("-")],
  ["dot.case", (w) => w.join(".")],
];

function tableRow(cells) {
  return `| ${cells.join(" | ")} |`;
}

export function handle(_toolId, input) {
  const text = String(input || "");
  if (!text.trim()) throw new Error(duck.t("请粘贴要转换的文本"));

  const lines = text.split(/\r?\n/).filter((line) => line.trim());
  const multi = lines.length > 1;
  const header = multi ? ["输入", "风格", "结果"] : ["风格", "结果"];
  const rows = [tableRow(header), tableRow(header.map(() => "---"))];

  for (const line of lines) {
    const words = splitWords(line).map((word) => word.toLowerCase());
    for (const [label, build] of STYLES) {
      const value = words.length ? build(words) : "";
      rows.push(tableRow(multi ? [line.trim(), label, value] : [label, value]));
    }
  }
  return rows.join("\n");
}
