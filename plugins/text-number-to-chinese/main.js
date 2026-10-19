// Duck Toolbox 插件：数字转中文（duck.text.number-to-chinese）。
//
// 契约：`handle(toolId, input)` 返回字符串结果；参数经 `duck.params` 注入。

const CN_DIGITS = ["零", "壹", "贰", "叁", "肆", "伍", "陆", "柒", "捌", "玖"];
const CN_UNITS = ["", "拾", "佰", "仟"];
const CN_BIG = ["", "万", "亿", "万亿"];

function fourDigitsToChinese(n) {
  let out = "";
  let zeroPending = false;
  for (let unit = 3; unit >= 0; unit--) {
    const digit = Math.floor(n / 10 ** unit) % 10;
    if (digit === 0) {
      zeroPending = out.length > 0;
    } else {
      if (zeroPending) out += "零";
      zeroPending = false;
      out += CN_DIGITS[digit] + CN_UNITS[unit];
    }
  }
  return out;
}

function numberToPlainChinese(num) {
  const negative = num < 0;
  let n = Math.floor(Math.abs(num));
  if (n === 0) return "零";
  const sections = [];
  while (n > 0) {
    sections.push(n % 10000);
    n = Math.floor(n / 10000);
  }
  let out = "";
  for (let i = sections.length - 1; i >= 0; i--) {
    if (sections[i] === 0) continue;
    out += fourDigitsToChinese(sections[i]) + CN_BIG[i];
  }
  return (negative ? "负" : "") + out;
}

function numberToRmbChinese(num) {
  const negative = num < 0;
  const abs = Math.abs(num);
  const yuan = Math.floor(abs);
  const cents = Math.round((abs - yuan) * 100);
  const jiao = Math.floor(cents / 10);
  const fen = cents % 10;
  let out = negative ? "负" : "";
  out += yuan === 0 ? "" : numberToPlainChinese(yuan) + "元";
  if (jiao === 0 && fen === 0) {
    return out === "" || out === "负" ? "零元整" : out + "整";
  }
  if (jiao > 0) out += CN_DIGITS[jiao] + "角";
  if (fen > 0) {
    if (jiao === 0 && yuan > 0) out += "零";
    out += CN_DIGITS[fen] + "分";
  }
  return out;
}

function doNumberToChinese(input) {
  const rows = ["| 数字 | 人民币大写 | 中文读法 |", "| --- | --- | --- |"];
  for (const raw of input.split(/\r?\n/)) {
    const text = raw.trim().replace(/[,，]/g, "");
    if (!text) continue;
    const num = Number(text);
    if (!Number.isFinite(num)) {
      rows.push(`| ${text} | ${duck.t("无法识别的数字")} |  |`);
      continue;
    }
    rows.push(`| ${text} | ${numberToRmbChinese(num)} | ${numberToPlainChinese(Math.abs(num))} |`);
  }
  if (rows.length === 2) throw new Error(duck.t("请输入数字，支持每行一个"));
  return rows.join("\n");
}

export function handle(_toolId, input) {
  return doNumberToChinese(String(input || ""));
}
